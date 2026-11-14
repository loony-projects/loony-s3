import { Link, useNavigate } from 'react-router-dom';
import { Database, LogOut, KeyRound, ShieldCheck, UserRound, Wallet } from 'lucide-react';
import { consoleApi } from '@/api/console';
import { identityProviderLogoutUrl } from '@/lib/oidc';
import { useAuthStore } from '@/store/useAuthStore';
import { useConsoleStore } from '@/store/useConsoleStore';

export function Navbar() {
  const { accessKey, logout } = useAuthStore();
  const session = useConsoleStore((s) => s.session);
  const clearSession = useConsoleStore((s) => s.clear);
  const navigate = useNavigate();

  async function handleLogout() {
    if (!session) {
      logout();
      navigate('/login');
      return;
    }
    // Ends the keys right away. If this fails, they still expire on their own, and it
    // shouldn't stop someone signing out.
    await consoleApi.signOut().catch(() => undefined);
    clearSession();
    const logoutUrl = identityProviderLogoutUrl();
    if (logoutUrl) window.location.assign(logoutUrl);
    else navigate('/login');
  }

  const who = session?.user.name ?? session?.user.email;

  return (
    <nav className="bg-white border-b border-gray-200 sticky top-0 z-40">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex items-center justify-between h-14 gap-3">
          <Link to="/" className="flex items-center gap-2 font-semibold text-gray-900 hover:text-blue-600 transition-colors">
            <Database className="w-5 h-5 text-blue-600" />
            <span className="whitespace-nowrap">Loony LS3</span>
          </Link>

          {(session || accessKey) && (
            <div className="flex items-center gap-4 min-w-0">
              {session && (
                <Link to="/billing" className="flex items-center gap-1.5 text-sm text-gray-600 hover:text-blue-600 transition-colors">
                  <Wallet className="w-4 h-4" />
                  <span>Billing</span>
                </Link>
              )}
              {session ? (
                <div className="hidden sm:flex items-center gap-2 text-sm text-gray-600 min-w-0" title={session.user.email ?? undefined}>
                  <UserRound className="w-4 h-4 shrink-0" />
                  <span className="truncate">{who}</span>
                </div>
              ) : (
                accessKey && (
                  <>
                    <Link to="/admin" className="flex items-center gap-1.5 text-sm text-gray-600 hover:text-blue-600 transition-colors">
                      <ShieldCheck className="w-4 h-4" />
                      <span>Admin</span>
                    </Link>
                    <div className="hidden sm:flex items-center gap-2 text-sm text-gray-600" title={accessKey}>
                      <KeyRound className="w-4 h-4" />
                      <span className="font-mono">{accessKey.slice(0, 12)}…</span>
                    </div>
                  </>
                )
              )}
              <button
                onClick={() => void handleLogout()}
                className="flex items-center gap-1.5 text-sm text-gray-500 hover:text-red-600 transition-colors shrink-0"
              >
                <LogOut className="w-4 h-4" />
                <span className="whitespace-nowrap">Sign out</span>
              </button>
            </div>
          )}
        </div>
      </div>
    </nav>
  );
}
