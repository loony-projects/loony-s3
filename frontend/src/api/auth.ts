import { signedFetch, ApiError } from './client';
import { parseS3Error } from './xml';

/**
 * SigV4 has no token-issuance endpoint -- unlike the old JWT flow, there's nothing to
 * exchange credentials for. "Signing in" here means verifying the given access/secret
 * key actually work against `endpoint` (a real signed ListBuckets call) before storing
 * them, so a typo shows up immediately instead of on the first later action.
 */
export async function verifyCredentials(
  accessKey: string,
  secretKey: string,
  region: string,
  endpoint: string,
): Promise<void> {
  const res = await signedFetch('GET', '/', { credentials: { accessKey, secretKey, region, endpoint } });
  if (res.ok) return;

  const body = await res.text().catch(() => '');
  const parsed = parseS3Error(body);
  throw new ApiError(res.status, parsed?.message || res.statusText || 'Could not reach the server', parsed?.code);
}
