import { Navigate, Route, Routes } from 'react-router-dom';
import { useAuthStore } from '@/store/useAuthStore';
import { Navbar } from '@/components/Navbar';
import { LoginPage } from '@/pages/LoginPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { BucketPage } from '@/pages/BucketPage';
import { ReactNode } from 'react';

function RequireAuth({ children }: { children: ReactNode }) {
  const isAuthenticated = useAuthStore((s) => s.isAuthenticated);
  return isAuthenticated ? <>{children}</> : <Navigate to="/login" replace />;
}

export function App() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route
        path="/*"
        element={
          <RequireAuth>
            <div className="min-h-screen bg-gray-50">
              <Navbar />
              <Routes>
                <Route path="/" element={<DashboardPage />} />
                <Route path="/buckets/:name" element={<BucketPage />} />
                <Route path="*" element={<Navigate to="/" replace />} />
              </Routes>
            </div>
          </RequireAuth>
        }
      />
    </Routes>
  );
}
