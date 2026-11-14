import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';

export const API_ENDPOINT = ((import.meta.env['VITE_API_URL'] as string | undefined) || 'http://localhost:7353').replace(/\/$/, '');
export const API_REGION = (import.meta.env['VITE_REGION'] as string | undefined) || 'us-east-1';

export interface ConsoleUser {
  id: string;
  email: string | null;
  email_verified: boolean;
  name: string | null;
}

export interface ConsoleSession {
  accessKey: string;
  secretKey: string;
  issuedAt: number;
  expiresAt: number;
  user: ConsoleUser;
}

export interface ProjectKey {
  projectId: string;
  accessKey: string;
  secretKey: string;
}

interface ConsoleState {
  session: ConsoleSession | null;
  /** The open project's key, which everything the bucket pages do is signed with. */
  projectKey: ProjectKey | null;
  /** Shown on the sign-in page until the next sign-in, to say why someone is back there. */
  notice: string | null;
  signIn: (session: ConsoleSession) => void;
  setUser: (user: ConsoleUser) => void;
  setProjectKey: (key: ProjectKey | null) => void;
  clear: (notice?: string) => void;
}

export const useConsoleStore = create<ConsoleState>()(
  persist(
    (set) => ({
      session: null,
      projectKey: null,
      notice: null,
      signIn: (session) => set({ session, projectKey: null, notice: null }),
      setUser: (user) => set((state) => (state.session ? { session: { ...state.session, user } } : state)),
      setProjectKey: (projectKey) => set({ projectKey }),
      clear: (notice) => set({ session: null, projectKey: null, notice: notice ?? null }),
    }),
    {
      name: 'loony-console',
      // This tab only, and gone when it closes. A project's key isn't even kept that long:
      // it's asked for again whenever a project is opened.
      storage: createJSONStorage(() => sessionStorage),
      partialize: (state) => ({ session: state.session, notice: state.notice }),
    },
  ),
);
