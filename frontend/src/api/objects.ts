import { request, presignedUrl, getCredentials, hostOf, requestUrl, ApiError } from './client';
import { amzDateNow, signHeaderAuth, buildQueryString } from './sigv4';
import {
  parseListBucketResult, parseLs3Error, parseUploadId, parseCompleteEtag,
  type ListBucketResult, type ObjectSummary,
} from './xml';
import { runPool, sleep } from '@/lib/pool';

export type { ListBucketResult, ObjectSummary };

const MiB = 1024 * 1024;
/** Files larger than this are uploaded in parts; smaller ones in a single PUT. */
export const MULTIPART_THRESHOLD = 16 * MiB;
const PART_SIZE = 8 * MiB;
const PART_CONCURRENCY = 4;
const PART_ATTEMPTS = 3;
const DELETE_CONCURRENCY = 8;

export function abortError(): ApiError {
  return new ApiError(0, 'Cancelled', 'Aborted');
}

/** True for both our own cancel error and a `fetch` aborted through its signal. */
export function isAbort(err: unknown): boolean {
  return (err instanceof ApiError && err.code === 'Aborted')
    || (err instanceof DOMException && err.name === 'AbortError');
}

export async function listObjects(
  bucket: string,
  prefix = '',
  delimiter = '/',
  continuationToken?: string,
): Promise<ListBucketResult> {
  const query: Record<string, string> = { 'list-type': '2' };
  if (prefix) query['prefix'] = prefix;
  if (delimiter) query['delimiter'] = delimiter;
  if (continuationToken) query['continuation-token'] = continuationToken;

  const res = await request('GET', `/${bucket}`, { query });
  return parseListBucketResult(await res.text());
}

/** Every key under `prefix`, recursively (no delimiter), across all result pages. */
export async function listAllKeys(bucket: string, prefix: string): Promise<string[]> {
  const keys: string[] = [];
  let token: string | undefined;
  do {
    const page = await listObjects(bucket, prefix, '', token);
    keys.push(...page.objects.map((o) => o.key));
    token = page.isTruncated ? page.nextContinuationToken : undefined;
  } while (token);
  return keys;
}

export async function deleteObject(bucket: string, key: string): Promise<void> {
  await request('DELETE', `/${bucket}/${key}`);
}

export interface BulkDeleteResult {
  deleted: number;
  failed: { key: string; message: string }[];
}

/**
 * Deletes each key with its own request (the server has no multi-object delete),
 * several at a time. Keeps going past individual failures and reports them.
 */
export async function deleteObjects(
  bucket: string,
  keys: readonly string[],
  onProgress?: (done: number, total: number) => void,
): Promise<BulkDeleteResult> {
  const result: BulkDeleteResult = { deleted: 0, failed: [] };
  let done = 0;
  await runPool(keys, DELETE_CONCURRENCY, async (key) => {
    try {
      await deleteObject(bucket, key);
      result.deleted++;
    } catch (err) {
      result.failed.push({ key, message: err instanceof ApiError ? err.message : 'Delete failed' });
    }
    onProgress?.(++done, keys.length);
  });
  return result;
}

export interface ObjectHead {
  contentType: string;
  contentLength: number;
  etag: string;
  lastModified: string;
}

export async function headObject(bucket: string, key: string): Promise<ObjectHead> {
  const res = await request('HEAD', `/${bucket}/${key}`);
  return {
    contentType: res.headers.get('content-type') ?? '',
    contentLength: Number(res.headers.get('content-length') ?? '0'),
    etag: (res.headers.get('etag') ?? '').replace(/^"|"$/g, ''),
    lastModified: res.headers.get('last-modified') ?? '',
  };
}

/**
 * A presigned URL computed entirely client-side (prompt §53): SigV4 presigning is
 * something the holder of a credential computes, so this never touches the network.
 * A PUT link lets whoever holds it upload to exactly this key until it expires.
 */
export async function presignObjectUrl(
  bucket: string,
  key: string,
  method: 'GET' | 'PUT' = 'GET',
  expiresSecs = 3600,
): Promise<string> {
  return presignedUrl(method, `/${bucket}/${key}`, expiresSecs);
}

/**
 * One signed PUT sent via XMLHttpRequest rather than `fetch`, purely for
 * `upload.onprogress` -- browsers have no progress event for a `fetch` request body.
 * Resolves with the response ETag (quotes stripped).
 */
async function xhrPut(
  path: string,
  query: Record<string, string>,
  body: Blob,
  contentType: string | undefined,
  onProgress?: (loaded: number) => void,
  signal?: AbortSignal,
): Promise<string> {
  const creds = getCredentials();
  const amzDate = amzDateNow();
  const payloadHash = 'UNSIGNED-PAYLOAD';
  const signingHeaders = { host: hostOf(creds.endpoint), 'x-amz-date': amzDate, 'x-amz-content-sha256': payloadHash };
  const authorization = await signHeaderAuth({
    method: 'PUT',
    path,
    query,
    headers: signingHeaders,
    signedHeaders: Object.keys(signingHeaders).sort(),
    payloadHash,
    accessKey: creds.accessKey,
    secretKey: creds.secretKey,
    region: creds.region,
    amzDate,
  });
  const url = requestUrl(creds.endpoint, path, buildQueryString(query));

  return new Promise<string>((resolve, reject) => {
    if (signal?.aborted) {
      reject(abortError());
      return;
    }
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', url);
    // "host" is browser-managed (same restriction as fetch); everything else that was
    // signed goes on exactly as signed.
    xhr.setRequestHeader('x-amz-date', amzDate);
    xhr.setRequestHeader('x-amz-content-sha256', payloadHash);
    xhr.setRequestHeader('authorization', authorization);
    if (contentType) xhr.setRequestHeader('Content-Type', contentType);

    const onAbort = () => xhr.abort();
    signal?.addEventListener('abort', onAbort, { once: true });
    const settle = () => signal?.removeEventListener('abort', onAbort);

    xhr.upload.onprogress = (e) => onProgress?.(e.loaded);
    xhr.onload = () => {
      settle();
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(xhr.getResponseHeader('etag')?.replace(/^"|"$/g, '') ?? '');
      } else {
        const parsed = parseLs3Error(xhr.responseText);
        reject(new ApiError(xhr.status, parsed?.message ?? xhr.statusText, parsed?.code));
      }
    };
    xhr.onerror = () => { settle(); reject(new ApiError(0, 'Network error')); };
    xhr.onabort = () => { settle(); reject(abortError()); };
    xhr.send(body);
  });
}

export interface UploadOptions {
  onProgress?: (loaded: number, total: number) => void;
  signal?: AbortSignal;
}

/**
 * Uploads `file` to `bucket/key`: a single PUT up to MULTIPART_THRESHOLD, a multipart
 * upload above it (parallel parts, each retried, abandoned upload aborted server-side
 * on failure or cancel).
 */
export async function uploadFile(
  bucket: string,
  key: string,
  file: File,
  opts: UploadOptions = {},
): Promise<{ etag: string }> {
  const contentType = file.type || 'application/octet-stream';
  if (file.size <= MULTIPART_THRESHOLD) {
    const etag = await xhrPut(`/${bucket}/${key}`, {}, file, contentType,
      (loaded) => opts.onProgress?.(loaded, file.size), opts.signal);
    return { etag };
  }
  return uploadMultipart(bucket, key, file, contentType, opts);
}

async function uploadMultipart(
  bucket: string,
  key: string,
  file: File,
  contentType: string,
  opts: UploadOptions,
): Promise<{ etag: string }> {
  const path = `/${bucket}/${key}`;
  const created = await request('POST', path, {
    query: { uploads: '' },
    extraHeaders: { 'content-type': contentType },
    signal: opts.signal,
  });
  const uploadId = parseUploadId(await created.text());
  if (!uploadId) throw new ApiError(0, 'Server did not return an upload id');

  try {
    const partCount = Math.ceil(file.size / PART_SIZE);
    const loaded = new Array<number>(partCount).fill(0);
    const etags = new Array<string>(partCount);
    const report = () => opts.onProgress?.(loaded.reduce((a, b) => a + b, 0), file.size);
    const partIndexes = Array.from({ length: partCount }, (_, i) => i);

    await runPool(partIndexes, PART_CONCURRENCY, async (i) => {
      const part = file.slice(i * PART_SIZE, Math.min(file.size, (i + 1) * PART_SIZE));
      for (let attempt = 1; ; attempt++) {
        try {
          etags[i] = await xhrPut(
            path, { partNumber: String(i + 1), uploadId }, part, undefined,
            (n) => { loaded[i] = n; report(); }, opts.signal,
          );
          loaded[i] = part.size;
          report();
          return;
        } catch (err) {
          if (isAbort(err) || attempt >= PART_ATTEMPTS) throw err;
          loaded[i] = 0;
          report();
          await sleep(500 * attempt);
        }
      }
    }, opts.signal);
    if (opts.signal?.aborted) throw abortError();

    const body = '<CompleteMultipartUpload>'
      + etags.map((etag, i) => `<Part><PartNumber>${i + 1}</PartNumber><ETag>"${etag}"</ETag></Part>`).join('')
      + '</CompleteMultipartUpload>';
    const completed = await request('POST', path, { query: { uploadId }, body, signal: opts.signal });
    return { etag: parseCompleteEtag(await completed.text()) };
  } catch (err) {
    // Best-effort: don't leave an abandoned upload (and its part shards) behind.
    await request('DELETE', path, { query: { uploadId } }).catch(() => undefined);
    throw isAbort(err) ? abortError() : err;
  }
}
