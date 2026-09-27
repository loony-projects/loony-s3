// Browser-native SigV4 request signing (canonical request, credential scope,
// HMAC-SHA256 signing-key chain), mirroring the Rust backend's implementation in
// crates/auth/src/canonical.rs and sign.rs byte-for-byte so requests signed here
// verify there. Uses only Web Crypto (`crypto.subtle`) -- no signing library needed,
// same reasoning as the backend using only `hmac`/`sha2` directly rather than a vendor
// SDK.

// Wire-protocol literals fixed by the SigV4 spec -- every standard client signs with
// these exact bytes, so they can't be renamed without breaking compatibility. This is
// the only place they're spelled out (mirrors crates/auth/src/canonical.rs).
const SIGNING_ALGORITHM = 'AWS4-HMAC-SHA256';
const SIGNING_KEY_PREFIX = 'AWS4';
const SCOPE_TERMINATOR = 'aws4_request';
const SERVICE = 's3';

/** `sha256("")` -- used as the payload hash for every empty-body request (GET/HEAD/DELETE). */
export const EMPTY_BODY_SHA256 = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

const textEncoder = new TextEncoder();

function toHex(buf: ArrayBuffer): string {
  return Array.from(new Uint8Array(buf))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

export async function sha256Hex(data: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', textEncoder.encode(data));
  return toHex(digest);
}

async function hmacSha256(key: BufferSource, data: string): Promise<ArrayBuffer> {
  const cryptoKey = await crypto.subtle.importKey('raw', key, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return crypto.subtle.sign('HMAC', cryptoKey, textEncoder.encode(data));
}

// Unreserved characters per RFC 3986 / SigV4 §"URI encode": pass through unescaped;
// everything else -- including each individual byte of a multi-byte UTF-8 sequence --
// becomes `%XX` uppercase hex. This must match crates/auth/src/canonical.rs's
// `uri_encode` exactly, since both sides need to derive the same canonical request.
const UNRESERVED = new Set<number>();
for (let c = 0x41; c <= 0x5a; c++) UNRESERVED.add(c); // A-Z
for (let c = 0x61; c <= 0x7a; c++) UNRESERVED.add(c); // a-z
for (let c = 0x30; c <= 0x39; c++) UNRESERVED.add(c); // 0-9
for (const c of ['-', '.', '_', '~']) UNRESERVED.add(c.charCodeAt(0));

export function uriEncode(input: string, encodeSlash: boolean): string {
  let out = '';
  for (const byte of textEncoder.encode(input)) {
    if (UNRESERVED.has(byte)) {
      out += String.fromCharCode(byte);
    } else if (byte === 0x2f /* '/' */ && !encodeSlash) {
      out += '/';
    } else {
      out += '%' + byte.toString(16).toUpperCase().padStart(2, '0');
    }
  }
  return out;
}

/**
 * Builds both the canonical query string (for signing) and the literal query string to
 * put on the wire, from the same sorted, `uriEncode`d pairs -- deliberately not
 * `URLSearchParams`, which form-encodes spaces as `+` rather than `%20` and would
 * desync from what the backend's RFC-3986 `percent_decode` expects.
 */
export function buildQueryString(params: Record<string, string>): string {
  const entries = Object.entries(params)
    .map(([k, v]) => [uriEncode(k, true), uriEncode(v, true)] as const)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return entries.map(([k, v]) => `${k}=${v}`).join('&');
}

function canonicalUri(rawPath: string): string {
  const encoded = uriEncode(rawPath, false);
  return encoded === '' ? '/' : encoded;
}

function canonicalHeaders(headers: Record<string, string>, signedHeaders: string[]): string {
  const byLowerName = new Map(Object.entries(headers).map(([k, v]) => [k.toLowerCase(), v]));
  const lines = signedHeaders
    .map((name) => [name.toLowerCase(), (byLowerName.get(name.toLowerCase()) ?? '').trim().replace(/\s+/g, ' ')] as const)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return lines.map(([k, v]) => `${k}:${v}\n`).join('');
}

async function buildCanonicalRequest(
  method: string,
  path: string,
  query: Record<string, string>,
  headers: Record<string, string>,
  signedHeaders: string[],
  payloadHash: string,
): Promise<string> {
  const parts = [
    method,
    canonicalUri(path),
    buildQueryString(query),
    canonicalHeaders(headers, signedHeaders),
    signedHeaders.join(';'),
    payloadHash,
  ];
  return parts.join('\n');
}

async function computeSignature(
  secretKey: string,
  date: string,
  region: string,
  stringToSign: string,
): Promise<string> {
  const kSecret = textEncoder.encode(`${SIGNING_KEY_PREFIX}${secretKey}`);
  const kDate = await hmacSha256(kSecret, date);
  const kRegion = await hmacSha256(kDate, region);
  const kService = await hmacSha256(kRegion, SERVICE);
  const kSigning = await hmacSha256(kService, SCOPE_TERMINATOR);
  const signature = await hmacSha256(kSigning, stringToSign);
  return toHex(signature);
}

/** The `x-amz-date` value for "now", in SigV4's `YYYYMMDDTHHMMSSZ` format. */
export function amzDateNow(): string {
  const d = new Date();
  const pad = (n: number) => n.toString().padStart(2, '0');
  return `${d.getUTCFullYear()}${pad(d.getUTCMonth() + 1)}${pad(d.getUTCDate())}T${pad(d.getUTCHours())}${pad(
    d.getUTCMinutes(),
  )}${pad(d.getUTCSeconds())}Z`;
}

export interface SignHeaderAuthParams {
  method: string;
  path: string;
  query?: Record<string, string>;
  /** Every header that will be signed, keyed by name exactly as it'll be sent (case-insensitive lookup internally). */
  headers: Record<string, string>;
  signedHeaders: string[];
  payloadHash: string;
  accessKey: string;
  secretKey: string;
  region: string;
  amzDate: string;
}

/** Signs a request and returns the value for its `Authorization` header. */
export async function signHeaderAuth(p: SignHeaderAuthParams): Promise<string> {
  const date = p.amzDate.slice(0, 8);
  const canonicalRequest = await buildCanonicalRequest(
    p.method,
    p.path,
    p.query ?? {},
    p.headers,
    p.signedHeaders,
    p.payloadHash,
  );
  const scope = `${date}/${p.region}/${SERVICE}/${SCOPE_TERMINATOR}`;
  const stringToSign = `${SIGNING_ALGORITHM}\n${p.amzDate}\n${scope}\n${await sha256Hex(canonicalRequest)}`;
  const signature = await computeSignature(p.secretKey, date, p.region, stringToSign);
  return `${SIGNING_ALGORITHM} Credential=${p.accessKey}/${scope}, SignedHeaders=${p.signedHeaders.join(';')}, Signature=${signature}`;
}

export interface SignPresignedQueryParams {
  method: string;
  path: string;
  headers: Record<string, string>;
  signedHeaders: string[];
  accessKey: string;
  secretKey: string;
  region: string;
  amzDate: string;
  expiresSecs: number;
  /** Any additional query parameters the request itself needs (e.g. none for plain GET). */
  extraQuery?: Record<string, string>;
}

/** Builds a presigned URL's full query string, including the final `X-Amz-Signature`. */
export async function signPresignedQuery(p: SignPresignedQueryParams): Promise<string> {
  const date = p.amzDate.slice(0, 8);
  const credential = `${p.accessKey}/${date}/${p.region}/${SERVICE}/${SCOPE_TERMINATOR}`;
  const query: Record<string, string> = {
    ...(p.extraQuery ?? {}),
    'X-Amz-Algorithm': SIGNING_ALGORITHM,
    'X-Amz-Credential': credential,
    'X-Amz-Date': p.amzDate,
    'X-Amz-Expires': String(p.expiresSecs),
    'X-Amz-SignedHeaders': p.signedHeaders.join(';'),
  };

  const canonicalRequest = await buildCanonicalRequest(p.method, p.path, query, p.headers, p.signedHeaders, 'UNSIGNED-PAYLOAD');
  const scope = `${date}/${p.region}/${SERVICE}/${SCOPE_TERMINATOR}`;
  const stringToSign = `${SIGNING_ALGORITHM}\n${p.amzDate}\n${scope}\n${await sha256Hex(canonicalRequest)}`;
  const signature = await computeSignature(p.secretKey, date, p.region, stringToSign);

  return buildQueryString({ ...query, 'X-Amz-Signature': signature });
}
