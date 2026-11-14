// The UI's settings come from the server at startup (`/config.json`), not from the build,
// so every setting lives in the server's configuration (scripts/.env) and one build works
// anywhere. Under `npm run dev`, Vite serves the same file from the scripts' settings.
//
// These are only set once `loadSettings` has finished. main.tsx waits for it before
// importing the rest of the app, so code elsewhere can read them as plain constants.

interface ServerSettings {
  api_url: string;
  region: string;
  admin_api_url: string;
  oidc_issuer: string | null;
  oidc_client_id: string | null;
}

/** The LS3 API, as the browser reaches it. */
export let API_URL = '';
/** SigV4 region. Matches the server's LS3_REGION. */
export let REGION = '';
/** The local-only admin API. */
export let ADMIN_API_URL = '';
/** Sign in with Loony: both set, or neither. */
export let OIDC_ISSUER: string | undefined;
export let OIDC_CLIENT_ID: string | undefined;

function text(value: unknown, name: string): string {
  if (typeof value !== 'string' || !value.trim()) throw new Error(`The server's /config.json has no ${name}.`);
  return value.trim();
}

export async function loadSettings(): Promise<void> {
  const response = await fetch('/config.json', { cache: 'no-store' });
  if (!response.ok) throw new Error(`Couldn't load this site's settings (HTTP ${response.status}).`);
  const settings = (await response.json()) as Partial<ServerSettings>;
  API_URL = text(settings.api_url, 'api_url').replace(/\/$/, '');
  REGION = text(settings.region, 'region');
  ADMIN_API_URL = text(settings.admin_api_url, 'admin_api_url').replace(/\/$/, '');
  const issuer = settings.oidc_issuer?.trim().replace(/\/$/, '') || undefined;
  const clientId = settings.oidc_client_id?.trim() || undefined;
  if (issuer && clientId) {
    OIDC_ISSUER = issuer;
    OIDC_CLIENT_ID = clientId;
  }
}
