import { FormEvent, useState } from 'react';
import { Navigate, useLocation, useNavigate } from 'react-router-dom';
import { Database } from 'lucide-react';
import { verifyCredentials } from '@/api/auth';
import { useAuthStore } from '@/store/useAuthStore';
import { useToast } from '@/context/ToastContext';
import { Spinner } from '@/components/Spinner';
import { ApiError } from '@/api/client';
import { safeReturnTo, signInEnabled, startSignIn } from '@/lib/oidc';
import { useConsoleStore } from '@/store/useConsoleStore';

const DEFAULT_ENDPOINT = (import.meta.env['VITE_API_URL'] as string | undefined) || 'http://localhost:7353';
const DEFAULT_REGION = (import.meta.env['VITE_REGION'] as string | undefined) || 'us-east-1';

export function LoginPage() {
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [region, setRegion] = useState(DEFAULT_REGION);
  const [endpoint, setEndpoint] = useState(DEFAULT_ENDPOINT);
  const [loading, setLoading] = useState(false);
  const { login: storeLogin } = useAuthStore();
  const navigate = useNavigate();
  const toast = useToast();
  const location = useLocation();
  const hasSession = useConsoleStore((state) => state.session !== null);
  const clearSession = useConsoleStore((state) => state.clear);
  const notice = useConsoleStore((state) => state.notice);
  const [showKeyForm, setShowKeyForm] = useState(!signInEnabled());
  const [redirecting, setRedirecting] = useState(false);
  const returnTo = safeReturnTo((location.state as { from?: string } | null)?.from);

  async function continueWithLoony(createAccount: boolean) {
    setRedirecting(true);
    try {
      await startSignIn({ returnTo, createAccount });
    } catch {
      toast.error('Could not start sign-in');
      setRedirecting(false);
    }
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!accessKey.trim() || !secretKey.trim() || !endpoint.trim()) return;

    setLoading(true);
    try {
      await verifyCredentials(accessKey.trim(), secretKey.trim(), region.trim() || 'us-east-1', endpoint.trim().replace(/\/$/, ''));
      clearSession();
      storeLogin(accessKey.trim(), secretKey.trim(), region.trim() || 'us-east-1', endpoint.trim().replace(/\/$/, ''));
      navigate('/');
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : 'Could not verify these credentials';
      toast.error(msg);
    } finally {
      setLoading(false);
    }
  }

  if (hasSession) return <Navigate to={returnTo} replace />;

  return (
    <div className="min-h-screen bg-gradient-to-br from-blue-50 to-indigo-100 flex items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <div className="text-center mb-8">
          <div className="inline-flex items-center justify-center w-14 h-14 bg-blue-600 rounded-2xl mb-4 shadow-lg shadow-blue-200">
            <Database className="w-7 h-7 text-white" />
          </div>
          <h1 className="text-2xl font-bold text-gray-900">Loony LS3</h1>
          <p className="text-sm text-gray-500 mt-1">Object Storage Dashboard</p>
        </div>

        {notice && (
          <p role="alert" className="mb-4 text-sm text-amber-800 bg-amber-50 border border-amber-200 rounded-xl px-4 py-3">{notice}</p>
        )}

        {signInEnabled() && (
          <div className="bg-white rounded-2xl shadow-sm border border-gray-200 p-6 mb-4 space-y-3">
            <button
              onClick={() => void continueWithLoony(false)}
              disabled={redirecting}
              className="w-full flex items-center justify-center gap-2 py-2.5 px-4 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors"
            >
              {redirecting && <Spinner size="sm" />}
              Sign in with Loony
            </button>
            <button
              onClick={() => void continueWithLoony(true)}
              disabled={redirecting}
              className="w-full py-2.5 px-4 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50 transition-colors"
            >
              Create account
            </button>
            {!showKeyForm && (
              <button onClick={() => setShowKeyForm(true)} className="w-full pt-1 text-xs text-gray-500 hover:text-gray-700 transition-colors">
                Sign in with an access key
              </button>
            )}
          </div>
        )}

        {showKeyForm && (
        <div className="bg-white rounded-2xl shadow-sm border border-gray-200 p-6">
          <form onSubmit={handleSubmit} className="space-y-4">
            <div>
              <label htmlFor="accessKey" className="block text-sm font-medium text-gray-700 mb-1.5">
                Access Key ID
              </label>
              <input
                id="accessKey"
                type="text"
                value={accessKey}
                onChange={(e) => setAccessKey(e.target.value)}
                placeholder="AKIA..."
                required
                autoComplete="username"
                className="w-full px-3 py-2.5 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent transition font-mono"
              />
            </div>

            <div>
              <label htmlFor="secretKey" className="block text-sm font-medium text-gray-700 mb-1.5">
                Secret Access Key
              </label>
              <input
                id="secretKey"
                type="password"
                value={secretKey}
                onChange={(e) => setSecretKey(e.target.value)}
                placeholder="••••••••••••••••••••••••••••••••"
                required
                autoComplete="current-password"
                className="w-full px-3 py-2.5 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent transition font-mono"
              />
            </div>

            <div className="grid grid-cols-2 gap-3">
              <div>
                <label htmlFor="region" className="block text-sm font-medium text-gray-700 mb-1.5">
                  Region
                </label>
                <input
                  id="region"
                  type="text"
                  value={region}
                  onChange={(e) => setRegion(e.target.value)}
                  placeholder="us-east-1"
                  className="w-full px-3 py-2.5 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent transition"
                />
              </div>
              <div>
                <label htmlFor="endpoint" className="block text-sm font-medium text-gray-700 mb-1.5">
                  Server URL
                </label>
                <input
                  id="endpoint"
                  type="text"
                  value={endpoint}
                  onChange={(e) => setEndpoint(e.target.value)}
                  placeholder="http://localhost:7353"
                  required
                  className="w-full px-3 py-2.5 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent transition"
                />
              </div>
            </div>

            <button
              type="submit"
              disabled={loading || !accessKey.trim() || !secretKey.trim() || !endpoint.trim()}
              className="w-full flex items-center justify-center gap-2 py-2.5 px-4 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors"
            >
              {loading ? <Spinner size="sm" /> : null}
              {loading ? 'Verifying…' : 'Sign in'}
            </button>
          </form>

          <p className="mt-4 text-xs text-center text-gray-400">
            Requests are signed with SigV4 — your secret key never leaves this browser
          </p>
        </div>
        )}
      </div>
    </div>
  );
}
