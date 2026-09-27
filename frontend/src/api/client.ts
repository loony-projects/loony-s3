import { useAuthStore } from '@/store/useAuthStore';
import { amzDateNow, signHeaderAuth, signPresignedQuery, sha256Hex, EMPTY_BODY_SHA256, buildQueryString } from './sigv4';
import { parseLs3Error } from './xml';

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
    public readonly code?: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

interface Credentials {
  accessKey: string;
  secretKey: string;
  region: string;
  endpoint: string;
}

export function getCredentials(): Credentials {
  const { accessKey, secretKey, region, endpoint } = useAuthStore.getState();
  if (!accessKey || !secretKey || !endpoint) {
    throw new ApiError(0, 'Not signed in');
  }
  return { accessKey, secretKey, region: region || 'us-east-1', endpoint };
}

function hostOf(endpoint: string): string {
  return new URL(endpoint).host;
}

export interface SignedRequestOptions {
  query?: Record<string, string>;
  /** String/Blob bodies are hashed and signed for real; omit for UNSIGNED-PAYLOAD (used for uploads, where hashing the whole file up front would defeat streaming). */
  body?: BodyInit;
  unsignedPayload?: boolean;
  extraHeaders?: Record<string, string>;
  credentials?: Credentials;
}

/**
 * Signs and sends one LS3 request. Returns the raw `Response` -- callers parse XML,
 * read headers, or stream the body as appropriate for that endpoint, the same
 * separation of concerns `loony-api`'s handlers keep on the server side.
 */
export async function signedFetch(method: string, path: string, opts: SignedRequestOptions = {}): Promise<Response> {
  const creds = opts.credentials ?? getCredentials();
  const amzDate = amzDateNow();
  const host = hostOf(creds.endpoint);

  let payloadHash: string;
  if (opts.unsignedPayload) {
    payloadHash = 'UNSIGNED-PAYLOAD';
  } else if (typeof opts.body === 'string') {
    payloadHash = await sha256Hex(opts.body);
  } else if (opts.body === undefined) {
    payloadHash = EMPTY_BODY_SHA256;
  } else {
    // Blob/ArrayBuffer/etc. bodies: hashing would mean buffering the whole thing
    // first, which defeats streaming for large uploads -- same tradeoff the backend
    // documents for why it trusts the declared x-amz-content-sha256 rather than
    // re-verifying it against the bytes it streams to disk.
    payloadHash = 'UNSIGNED-PAYLOAD';
  }

  // "host" must be part of what's signed, but browsers forbid setting it explicitly
  // via fetch/XHR -- the browser sends the real one automatically, and it matches
  // what we compute here since both come from the same `endpoint`.
  const signingHeaders: Record<string, string> = {
    host,
    'x-amz-date': amzDate,
    'x-amz-content-sha256': payloadHash,
    ...opts.extraHeaders,
  };
  const signedHeaders = Object.keys(signingHeaders)
    .map((h) => h.toLowerCase())
    .sort();

  const query = opts.query ?? {};
  const authorization = await signHeaderAuth({
    method,
    path,
    query,
    headers: signingHeaders,
    signedHeaders,
    payloadHash,
    accessKey: creds.accessKey,
    secretKey: creds.secretKey,
    region: creds.region,
    amzDate,
  });

  const fetchHeaders: Record<string, string> = { ...opts.extraHeaders, 'x-amz-date': amzDate, 'x-amz-content-sha256': payloadHash, authorization };

  const qs = buildQueryString(query);
  const url = `${creds.endpoint}${path}${qs ? `?${qs}` : ''}`;

  return fetch(url, { method, headers: fetchHeaders, body: opts.body });
}

/** `signedFetch`, but throwing `ApiError` (parsed from the LS3 `<Error>` body when
 * present) for any non-2xx/3xx/204 response, matching the old client's ergonomics. */
export async function request(method: string, path: string, opts: SignedRequestOptions = {}): Promise<Response> {
  const res = await signedFetch(method, path, opts);
  if (res.ok) return res;

  const body = await res.text().catch(() => '');
  const parsed = parseLs3Error(body);
  throw new ApiError(res.status, parsed?.message || res.statusText, parsed?.code);
}

export async function presignedGetUrl(path: string, expiresSecs: number): Promise<string> {
  const creds = getCredentials();
  const host = hostOf(creds.endpoint);
  const qs = await signPresignedQuery({
    method: 'GET',
    path,
    headers: { host },
    signedHeaders: ['host'],
    accessKey: creds.accessKey,
    secretKey: creds.secretKey,
    region: creds.region,
    amzDate: amzDateNow(),
    expiresSecs,
  });
  return `${creds.endpoint}${path}?${qs}`;
}
