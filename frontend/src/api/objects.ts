import { request, presignedGetUrl, getCredentials, ApiError } from './client';
import { amzDateNow, signHeaderAuth, buildQueryString } from './sigv4';
import { parseListBucketResult, parseLs3Error, type ListBucketResult, type ObjectSummary } from './xml';

export type { ListBucketResult, ObjectSummary };

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

export async function deleteObject(bucket: string, key: string): Promise<void> {
  await request('DELETE', `/${bucket}/${key}`);
}

/**
 * A real client-side-signed presigned URL (prompt §53) -- unlike the old API, there is
 * no server endpoint that generates one; SigV4 presigning is something the holder of a
 * credential computes themselves, so this never touches the network.
 */
export async function presignGetUrl(bucket: string, key: string, expiresSecs = 3600): Promise<string> {
  return presignedGetUrl(`/${bucket}/${key}`, expiresSecs);
}

/**
 * Always a single PUT, regardless of file size: the backend doesn't implement
 * multipart upload yet (a later, separately-scoped phase), so there is no chunking
 * path to fall back to here. Uses XMLHttpRequest instead of `fetch` purely for
 * `upload.onprogress` -- the browser has no progress event for a `fetch` request body.
 */
export function uploadObject(
  bucket: string,
  key: string,
  file: File,
  onProgress?: (pct: number) => void,
): Promise<{ etag: string }> {
  const creds = getCredentials();
  const path = `/${bucket}/${key}`;

  return (async () => {
    const amzDate = amzDateNow();
    const host = new URL(creds.endpoint).host;
    const payloadHash = 'UNSIGNED-PAYLOAD';
    const signingHeaders = { host, 'x-amz-date': amzDate, 'x-amz-content-sha256': payloadHash };
    const signedHeaders = Object.keys(signingHeaders).sort();
    const authorization = await signHeaderAuth({
      method: 'PUT',
      path,
      headers: signingHeaders,
      signedHeaders,
      payloadHash,
      accessKey: creds.accessKey,
      secretKey: creds.secretKey,
      region: creds.region,
      amzDate,
    });

    const qs = buildQueryString({});
    const url = `${creds.endpoint}${path}${qs ? `?${qs}` : ''}`;

    return new Promise<{ etag: string }>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open('PUT', url);
      // "host" is browser-managed (same restriction as fetch); everything else that
      // was signed goes on exactly as signed.
      xhr.setRequestHeader('x-amz-date', amzDate);
      xhr.setRequestHeader('x-amz-content-sha256', payloadHash);
      xhr.setRequestHeader('authorization', authorization);
      xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream');

      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && onProgress) onProgress(Math.round((e.loaded / e.total) * 100));
      };
      xhr.onload = () => {
        if (xhr.status >= 200 && xhr.status < 300) {
          resolve({ etag: xhr.getResponseHeader('etag')?.replace(/^"|"$/g, '') ?? '' });
        } else {
          const parsed = parseLs3Error(xhr.responseText);
          reject(new ApiError(xhr.status, parsed?.message ?? xhr.statusText, parsed?.code));
        }
      };
      xhr.onerror = () => reject(new ApiError(0, 'Network error'));
      xhr.send(file);
    });
  })();
}
