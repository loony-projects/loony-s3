import { ReactNode, useEffect } from 'react';
import { Navigate, Route, Routes, useLocation } from 'react-router-dom';
import { useAuthStore } from '@/store/useAuthStore';
import { useConsoleStore } from '@/store/useConsoleStore';
import { Navbar } from '@/components/Navbar';
import { renewSession, sessionRenewDue } from '@/lib/oidc';
import { CallbackPage } from '@/pages/CallbackPage';
import { LoginPage } from '@/pages/LoginPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { BucketPage } from '@/pages/BucketPage';
import { ProjectsPage } from '@/pages/ProjectsPage';
import { BillingPage } from '@/pages/BillingPage';
import { CheckoutPage } from '@/pages/CheckoutPage';
import { ProjectLayout } from '@/pages/project/ProjectLayout';
import { AdminLayout } from '@/pages/admin/AdminLayout';

const RENEW_CHECK_MS = 30 * 1000;

/** Renews a session shortly before it expires, whenever nothing would be interrupted. A
 * tab in the background waits until it's looked at again. */
function useSessionRenewal(active: boolean) {
  useEffect(() => {
    if (!active) return;
    const check = () => {
      if (document.visibilityState === 'visible' && sessionRenewDue()) renewSession();
    };
    check();
    const timer = window.setInterval(check, RENEW_CHECK_MS);
    document.addEventListener('visibilitychange', check);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', check);
    };
  }, [active]);
}

function RequireAuth({ children }: { children: ReactNode }) {
  const isAuthenticated = useAuthStore((s) => s.isAuthenticated);
  const hasSession = useConsoleStore((s) => s.session !== null);
  const location = useLocation();
  useSessionRenewal(hasSession);
  if (hasSession || isAuthenticated) return <>{children}</>;
  return <Navigate to="/login" replace state={{ from: location.pathname + location.search }} />;
}

/** What someone signed in with their account sees. */
function ConsoleRoutes() {
  return (
    <Routes>
      <Route path="/" element={<ProjectsPage />} />
      <Route path="/projects/:projectId/*" element={<ProjectLayout />} />
      <Route path="/billing" element={<BillingPage />} />
      <Route path="/billing/checkout/:checkoutId" element={<CheckoutPage />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

/** What someone signed in with an access key sees, such as the root key. */
function AccessKeyRoutes() {
  return (
    <Routes>
      <Route path="/" element={<DashboardPage />} />
      <Route path="/buckets/:name" element={<BucketPage />} />
      <Route path="/admin/*" element={<AdminLayout />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

export function App() {
  const hasSession = useConsoleStore((s) => s.session !== null);
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/callback" element={<CallbackPage />} />
      <Route
        path="/*"
        element={
          <RequireAuth>
            <div className="min-h-screen bg-gray-50">
              <Navbar />
              {hasSession ? <ConsoleRoutes /> : <AccessKeyRoutes />}
            </div>
          </RequireAuth>
        }
      />
    </Routes>
  );
}
