import { Link, useNavigate } from 'react-router-dom';
import { Database, LogOut, KeyRound } from 'lucide-react';
import { useAuthStore } from '@/store/useAuthStore';

export function Navbar() {
  const { accessKey, logout } = useAuthStore();
  const navigate = useNavigate();

  function handleLogout() {
    logout();
    navigate('/login');
  }

  return (
    <nav className="bg-white border-b border-gray-200 sticky top-0 z-40">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex items-center justify-between h-14">
          <Link to="/" className="flex items-center gap-2 font-semibold text-gray-900 hover:text-blue-600 transition-colors">
            <Database className="w-5 h-5 text-blue-600" />
            <span>Loony S3</span>
          </Link>

          {accessKey && (
            <div className="flex items-center gap-4">
              <div className="flex items-center gap-2 text-sm text-gray-600" title={accessKey}>
                <KeyRound className="w-4 h-4" />
                <span className="font-mono">{accessKey.slice(0, 12)}…</span>
              </div>
              <button
                onClick={handleLogout}
                className="flex items-center gap-1.5 text-sm text-gray-500 hover:text-red-600 transition-colors"
              >
                <LogOut className="w-4 h-4" />
                <span>Sign out</span>
              </button>
            </div>
          )}
        </div>
      </div>
    </nav>
  );
}
