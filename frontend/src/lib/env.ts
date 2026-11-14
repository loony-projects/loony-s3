// Every setting this UI needs comes from frontend/.env.local (or the build's environment);
// there are no built-in fallbacks. See frontend/.env.example.

function required(name: string): string {
  const value = (import.meta.env[name] as string | undefined)?.trim();
  if (!value) {
    throw new Error(`${name} isn't set. Add it to frontend/.env.local (see frontend/.env.example) and restart or rebuild.`);
  }
  return value;
}

function optional(name: string): string | undefined {
  return (import.meta.env[name] as string | undefined)?.trim() || undefined;
}

/** The LS3 API, as the browser reaches it. */
export const API_URL = required('VITE_API_URL').replace(/\/$/, '');
/** SigV4 region. Must match the server's LS3_REGION. */
export const REGION = required('VITE_REGION');
/** The local-only admin API. */
export const ADMIN_API_URL = required('VITE_ADMIN_API_URL').replace(/\/$/, '');
/** Sign in with Loony: both set, or neither. */
export const OIDC_ISSUER = optional('VITE_OIDC_ISSUER')?.replace(/\/$/, '');
export const OIDC_CLIENT_ID = optional('VITE_OIDC_CLIENT_ID');
