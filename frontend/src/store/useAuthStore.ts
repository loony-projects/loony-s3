import { create } from 'zustand';
import { persist } from 'zustand/middleware';

interface AuthState {
  accessKey: string | null;
  secretKey: string | null;
  region: string | null;
  endpoint: string | null;
  isAuthenticated: boolean;
  login: (accessKey: string, secretKey: string, region: string, endpoint: string) => void;
  logout: () => void;
}

export const useAuthStore = create<AuthState>()(
  persist(
    (set) => ({
      accessKey: null,
      secretKey: null,
      region: null,
      endpoint: null,
      isAuthenticated: false,
      login: (accessKey, secretKey, region, endpoint) =>
        set({ accessKey, secretKey, region, endpoint, isAuthenticated: true }),
      logout: () => set({ accessKey: null, secretKey: null, region: null, endpoint: null, isAuthenticated: false }),
    }),
    { name: 'loony-s3-auth' },
  ),
);
