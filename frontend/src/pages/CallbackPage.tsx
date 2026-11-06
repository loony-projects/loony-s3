import { useEffect, useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router-dom';
import { ApiError } from '@/api/client';
import { createSession } from '@/api/console';
import { Spinner } from '@/components/Spinner';
import { PageError } from '@/components/PageError';
import { redirectUri, safeReturnTo, takeTransaction } from '@/lib/oidc';
import { useAuthStore } from '@/store/useAuthStore';
import { ConsoleSession, useConsoleStore } from '@/store/useConsoleStore';

interface Completed {
  session: ConsoleSession;
  returnTo: string;
}

/** A code only works once, and development mode runs effects twice. This is per code, so
 * running twice shares one exchange rather than failing the second time. */
const exchanges = new Map<string, Promise<Completed>>();

function exchange(code: string, state: string | null): Promise<Completed> {
  let pending = exchanges.get(code);
  if (!pending) {
    pending = (async () => {
      const transaction = takeTransaction();
      if (!transaction || !state || transaction.state !== state) {
        throw new ApiError(0, 'This sign-in link is out of date or was opened in a different tab. Please start again.');
      }
      const session = await createSession(code, transaction.verifier, redirectUri(), transaction.nonce);
      return { session, returnTo: safeReturnTo(transaction.returnTo) };
    })();
    exchanges.set(code, pending);
  }
  return pending;
}

export function CallbackPage() {
  const [params] = useSearchParams();
  const navigate = useNavigate();
  const signIn = useConsoleStore((s) => s.signIn);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let current = true;
    const code = params.get('code');
    if (!code) {
      setError(params.get('error') === 'access_denied' ? 'Sign-in was cancelled.' : 'Sign-in failed. Please try again.');
      return;
    }
    exchange(code, params.get('state'))
      .then(({ session, returnTo }) => {
        if (!current) return;
        // Only one way of being signed in at a time, so a saved root key doesn't quietly
        // come back after signing out of this account.
        useAuthStore.getState().logout();
        signIn(session);
        navigate(returnTo, { replace: true });
      })
      .catch((err: unknown) => {
        if (current) setError(err instanceof ApiError ? err.message : 'Sign-in failed. Please try again.');
      });
    return () => {
      current = false;
    };
  }, [params, navigate, signIn]);

  return (
    <div className="min-h-screen bg-gray-50 flex items-center justify-center p-4">
      <div className="w-full max-w-md">
        {error ? (
          <div className="space-y-4">
            <PageError title="Couldn't sign you in">{error}</PageError>
            <Link to="/login" className="block text-center text-sm font-medium text-blue-600 hover:text-blue-700">Back to sign in</Link>
          </div>
        ) : (
          <div className="flex flex-col items-center gap-3 text-sm text-gray-500" role="status">
            <Spinner size="lg" />
            Signing you in…
          </div>
        )}
      </div>
    </div>
  );
}
