import { isBusy } from '@/lib/activity';
import { OIDC_CLIENT_ID as CLIENT_ID, OIDC_ISSUER as ISSUER } from '@/lib/env';
import { useConsoleStore } from '@/store/useConsoleStore';

const TRANSACTION_KEY = 'loony-oidc-transaction';
const LAST_RENEWAL_KEY = 'loony-last-renewal';

/** However a session comes to look due straight after renewing, such as a badly wrong
 * clock, this stops the page redirecting in an endless loop. */
const MIN_RENEWAL_INTERVAL_MS = 60 * 1000;

/** How close to expiry a session is renewed, at most. Short sessions renew proportionally
 * sooner, so one shorter than this doesn't renew in an endless loop. */
const MAX_RENEW_MARGIN_MS = 5 * 60 * 1000;

export interface SignInTransaction {
  state: string;
  nonce: string;
  verifier: string;
  returnTo: string;
}

export function signInEnabled(): boolean {
  return Boolean(ISSUER && CLIENT_ID);
}

export function redirectUri(): string {
  return `${window.location.origin}/callback`;
}

function base64Url(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function randomToken(): string {
  return base64Url(crypto.getRandomValues(new Uint8Array(32)));
}

/** Only a path on this site: never let a crafted return address send someone elsewhere. */
export function safeReturnTo(path: string | null | undefined): string {
  if (!path || !path.startsWith('/') || path.startsWith('//') || path.includes('\\') || path.startsWith('/callback') || path.startsWith('/login')) {
    return '/';
  }
  return path;
}

export async function startSignIn(options: { returnTo?: string; createAccount?: boolean } = {}): Promise<void> {
  if (!ISSUER || !CLIENT_ID) throw new Error('Sign-in is not configured');

  const transaction: SignInTransaction = {
    state: randomToken(),
    nonce: randomToken(),
    verifier: randomToken(),
    returnTo: safeReturnTo(options.returnTo),
  };
  sessionStorage.setItem(TRANSACTION_KEY, JSON.stringify(transaction));

  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(transaction.verifier));
  const params = new URLSearchParams({
    response_type: 'code',
    client_id: CLIENT_ID,
    redirect_uri: redirectUri(),
    scope: 'openid profile email',
    state: transaction.state,
    nonce: transaction.nonce,
    code_challenge: base64Url(new Uint8Array(digest)),
    code_challenge_method: 'S256',
  });
  if (options.createAccount) params.set('prompt', 'create');
  window.location.assign(`${ISSUER}/oauth/authorize?${params}`);
}

/** Takes the pending sign-in, so a transaction can only be completed once. */
export function takeTransaction(): SignInTransaction | null {
  const raw = sessionStorage.getItem(TRANSACTION_KEY);
  sessionStorage.removeItem(TRANSACTION_KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as SignInTransaction;
  } catch {
    return null;
  }
}

/** Ends the identity provider's session too. Otherwise the next "Sign in" would sign
 * straight back in without asking. */
export function identityProviderLogoutUrl(): string | null {
  if (!ISSUER || !CLIENT_ID) return null;
  const params = new URLSearchParams({
    client_id: CLIENT_ID,
    post_logout_redirect_uri: `${window.location.origin}/login`,
  });
  return `${ISSUER}/oauth/logout?${params}`;
}

export function sessionRenewDue(now = Date.now()): boolean {
  const { session } = useConsoleStore.getState();
  if (!session) return false;
  const margin = Math.min(MAX_RENEW_MARGIN_MS, (session.expiresAt - session.issuedAt) / 10);
  return now >= session.expiresAt - margin;
}

/** Renews through the identity provider, which normally doesn't ask for a password, and
 * comes back to this page. Waits rather than interrupting running work. Returns whether
 * it's redirecting. */
export function renewSession(): boolean {
  if (isBusy()) return false;
  const last = Number(sessionStorage.getItem(LAST_RENEWAL_KEY) ?? 0);
  if (Date.now() - last < MIN_RENEWAL_INTERVAL_MS) return false;
  sessionStorage.setItem(LAST_RENEWAL_KEY, String(Date.now()));
  void startSignIn({ returnTo: window.location.pathname + window.location.search });
  return true;
}
