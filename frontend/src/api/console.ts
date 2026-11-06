import { ApiError, signedFetch } from './client';
import { renewSession } from '@/lib/oidc';
import { API_ENDPOINT, API_REGION, ConsoleSession, ConsoleUser, useConsoleStore } from '@/store/useConsoleStore';

export type ProjectRole = 'owner' | 'member';

export interface Project {
  id: string;
  name: string;
  enabled: boolean;
  role: ProjectRole;
  created_by_me: boolean;
  created_at: string;
}

export interface IncomingInvite {
  id: string;
  project_name: string;
  role: ProjectRole;
  expires_at: string;
}

export interface ProjectList {
  created: Project[];
  joined: Project[];
  invites: IncomingInvite[];
}

export interface AccessKey {
  access_key: string;
  name: string | null;
  enabled: boolean;
  created_by: string | null;
  created_at: string;
  last_used_at: string | null;
}

export interface CreatedAccessKey extends AccessKey {
  secret_key: string;
}

export interface Member {
  user_id: string;
  email: string | null;
  name: string | null;
  role: ProjectRole;
  added_at: string;
}

export interface Invite {
  id: string;
  project_id: string;
  email: string;
  role: ProjectRole;
  invited_by: string;
  created_at: string;
  expires_at: string;
}

interface OpenedProject {
  access_key: string;
  secret_key: string;
  expires_at: string;
}

interface CreatedSession {
  access_key: string;
  secret_key: string;
  expires_at: string;
  user: ConsoleUser;
}

async function readError(res: Response): Promise<ApiError> {
  let message = res.statusText;
  let code: string | undefined;
  try {
    const body = (await res.json()) as { code?: string; message?: string };
    message = body.message || message;
    code = body.code;
  } catch {
    // Not JSON: keep the status text.
  }
  return new ApiError(res.status, message, code);
}

function handleAuthFailure(error: ApiError) {
  const { clear } = useConsoleStore.getState();
  if (error.code === 'session_expired') {
    renewSession();
  } else if (error.code === 'account_disabled') {
    clear(error.message);
  } else if (error.code === 'unauthorized') {
    clear('You were signed out. Sign in again to continue.');
  }
}

async function consoleRequest<T>(method: string, path: string, json?: unknown): Promise<T> {
  const session = useConsoleStore.getState().session;
  if (!session) throw new ApiError(0, 'Not signed in');

  const hasBody = json !== undefined;
  let res: Response;
  try {
    res = await signedFetch(method, `/_console/v1${path}`, {
      credentials: { accessKey: session.accessKey, secretKey: session.secretKey, region: API_REGION, endpoint: API_ENDPOINT },
      body: hasBody ? JSON.stringify(json) : undefined,
      extraHeaders: hasBody ? { 'content-type': 'application/json' } : undefined,
    });
  } catch {
    throw new ApiError(0, "Can't reach the server. Check your connection and try again.");
  }

  if (!res.ok) {
    const error = await readError(res);
    handleAuthFailure(error);
    throw error;
  }
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** Swaps a one-time code for a session key. The only request that isn't signed. */
export async function createSession(code: string, verifier: string, redirectUri: string, nonce: string): Promise<ConsoleSession> {
  let res: Response;
  try {
    res = await fetch(`${API_ENDPOINT}/_console/v1/session`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ code, code_verifier: verifier, redirect_uri: redirectUri, nonce }),
    });
  } catch {
    throw new ApiError(0, "Can't reach the server. Check your connection and try again.");
  }
  if (!res.ok) throw await readError(res);
  const created = (await res.json()) as CreatedSession;
  return {
    accessKey: created.access_key,
    secretKey: created.secret_key,
    issuedAt: Date.now(),
    expiresAt: Date.parse(created.expires_at),
    user: created.user,
  };
}

const p = (id: string) => `/projects/${encodeURIComponent(id)}`;

export const consoleApi = {
  me: () => consoleRequest<ConsoleUser>('GET', '/me'),
  signOut: () => consoleRequest<void>('DELETE', '/session'),

  projects: () => consoleRequest<ProjectList>('GET', '/projects'),
  createProject: (name: string) => consoleRequest<Project>('POST', '/projects', { name }),
  project: (id: string) => consoleRequest<Project>('GET', p(id)),
  renameProject: (id: string, name: string) => consoleRequest<Project>('PATCH', p(id), { name }),
  deleteProject: (id: string) => consoleRequest<void>('DELETE', p(id)),
  leaveProject: (id: string) => consoleRequest<void>('POST', `${p(id)}/leave`),
  openProject: (id: string) => consoleRequest<OpenedProject>('POST', `${p(id)}/session`),

  accessKeys: (id: string) => consoleRequest<AccessKey[]>('GET', `${p(id)}/keys`),
  createAccessKey: (id: string, name: string) => consoleRequest<CreatedAccessKey>('POST', `${p(id)}/keys`, { name }),
  setAccessKeyEnabled: (id: string, key: string, enabled: boolean) =>
    consoleRequest<AccessKey>('PATCH', `${p(id)}/keys/${encodeURIComponent(key)}`, { enabled }),
  deleteAccessKey: (id: string, key: string) => consoleRequest<void>('DELETE', `${p(id)}/keys/${encodeURIComponent(key)}`),

  members: (id: string) => consoleRequest<Member[]>('GET', `${p(id)}/members`),
  setMemberRole: (id: string, user: string, role: ProjectRole) => consoleRequest<Member>('PATCH', `${p(id)}/members/${user}`, { role }),
  removeMember: (id: string, user: string) => consoleRequest<void>('DELETE', `${p(id)}/members/${user}`),

  invites: (id: string) => consoleRequest<Invite[]>('GET', `${p(id)}/invites`),
  invite: (id: string, email: string, role: ProjectRole) => consoleRequest<Invite>('POST', `${p(id)}/invites`, { email, role }),
  cancelInvite: (id: string, invite: string) => consoleRequest<void>('DELETE', `${p(id)}/invites/${invite}`),
  acceptInvite: (invite: string) => consoleRequest<Project>('POST', `/invites/${invite}/accept`),
  declineInvite: (invite: string) => consoleRequest<void>('POST', `/invites/${invite}/decline`),
};
