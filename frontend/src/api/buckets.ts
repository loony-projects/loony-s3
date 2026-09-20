import { request } from './client';
import { parseListAllMyBuckets, type BucketSummary } from './xml';

export type Bucket = BucketSummary;

// Note: the backend has no bucket ACL concept (architecture.md's object model has no
// ACL field at all -- authorization is ownership-based, prompt §55), so there's
// nothing to select or display here. The old UI's ACL picker is gone rather than kept
// as a control that silently does nothing.

export async function listBuckets(): Promise<Bucket[]> {
  const res = await request('GET', '/');
  return parseListAllMyBuckets(await res.text());
}

// Paths passed to `request()` must be the *raw* logical path, never pre-encoded:
// `signedFetch` (via `uriEncode` in sigv4.ts) is the only percent-encoding pass, both
// for the canonical request it signs and the URL it actually sends. Pre-encoding here
// would double-escape (`%20` -> `%2520`) and desync the two. Bucket names are already
// restricted to a safe charset by S3 naming rules, so this is just consistency with
// how object keys (arbitrary bytes) must be handled in objects.ts.

export async function createBucket(name: string): Promise<void> {
  await request('PUT', `/${name}`);
}

export async function deleteBucket(name: string): Promise<void> {
  await request('DELETE', `/${name}`);
}
