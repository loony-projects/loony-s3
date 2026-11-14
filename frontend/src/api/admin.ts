import { ADMIN_API_URL } from '@/lib/env';
import { ApiError, getCredentials, signedFetch } from './client';

/** The admin API only listens on loopback, separately from the LS3 endpoint used to sign in. */
export const ADMIN_ENDPOINT = ADMIN_API_URL;

async function adminRequest<T>(method: string, path: string, opts: { query?: Record<string, string>; json?: unknown } = {}): Promise<T> {
  const credentials = { ...getCredentials(), endpoint: ADMIN_ENDPOINT };
  const hasBody = opts.json !== undefined;

  let res: Response;
  try {
    res = await signedFetch(method, `/admin/v1${path}`, {
      credentials,
      query: opts.query,
      body: hasBody ? JSON.stringify(opts.json) : undefined,
      extraHeaders: hasBody ? { 'content-type': 'application/json' } : undefined,
    });
  } catch {
    // Status 0: the browser couldn't connect, or refused the response under CORS.
    throw new ApiError(0, `Can't reach the admin API at ${ADMIN_ENDPOINT}`);
  }

  const text = await res.text();
  if (!res.ok) {
    let message = res.statusText;
    try {
      message = (JSON.parse(text) as { message?: string }).message || message;
    } catch {
      // Not JSON: keep the status text.
    }
    throw new ApiError(res.status, message);
  }
  return (text ? JSON.parse(text) : undefined) as T;
}

export interface Overview {
  mode: string;
  node_id: string;
  cluster_id: string | null;
  region: string;
  version: string;
  started_at: string;
  uptime_seconds: number;
  metadata: { state: string; term: number; leader: string | null; voters: number; learners: number };
  totals: {
    buckets: number;
    objects: number;
    bytes: number;
    owners: number;
    credentials: number;
    enabled_credentials: number;
    multipart_uploads: number;
    nodes: number;
  };
  nodes_by_state: Record<string, number>;
}

export type NodeState = 'Joining' | 'Healthy' | 'Suspect' | 'Offline' | 'Draining' | 'Removed';

export interface AdminNode {
  node_id: string;
  advertised_address: string;
  state: NodeState;
  generation: number;
  last_seen: string;
  volumes: number;
  shards: number;
  is_self: boolean;
  is_leader: boolean;
  is_voter: boolean;
}

export interface AdminBucket {
  name: string;
  owner_id: string;
  region: string;
  created_at: string;
  object_count: number;
  total_bytes: number;
}

export interface AdminObject {
  key: string;
  size: number;
  etag: string;
  last_modified: string;
}

export interface ObjectPage {
  folders: string[];
  objects: AdminObject[];
  next_cursor: string | null;
}

export interface AdminCredential {
  access_key: string;
  owner_id: string;
  enabled: boolean;
  admin: boolean;
  created_at: string;
  project_id: string | null;
  name: string | null;
}

export interface CreatedCredential extends AdminCredential {
  secret_key: string;
}

export interface AuditEntry {
  at: string;
  actor: string;
  action: string;
  target: string;
  outcome: 'ok' | 'denied' | 'failed';
  detail?: string;
}

const seg = encodeURIComponent;

export interface AdminUser {
  id: string;
  email: string | null;
  email_verified: boolean;
  name: string | null;
  enabled: boolean;
  created_at: string;
  last_sign_in_at: string;
}

/** A user from before an identity provider reset, and the one the same person got after. */
export interface RelinkCandidate {
  from: AdminUser;
  into: AdminUser;
}

export interface AdminProject {
  id: string;
  name: string;
  enabled: boolean;
  owner_id: string;
  created_at: string;
  members: { user_id: string; email: string | null; name: string | null; role: 'owner' | 'member' }[];
  access_keys: number;
  buckets: number;
  objects: number;
  bytes: number;
  subscription: 'unpaid' | 'active' | 'grace' | 'read_only' | 'suspended';
  plan_name: string | null;
  limit_bytes: number | null;
  period_end: string | null;
  cancel_at_period_end: boolean;
}

export interface GiftCode {
  id: string;
  hint: string;
  amount_paise: number;
  max_uses: number;
  uses: number;
  disabled: boolean;
  expires_at: string | null;
  note: string | null;
  created_by: string;
  created_at: string;
  redemptions: { user_id: string; email: string | null; amount_paise: number; at: string }[];
}

export interface AdoptableOwner {
  owner_id: string;
  access_keys: number;
  buckets: number;
  objects: number;
  bytes: number;
}

export const adminApi = {
  users: () => adminRequest<AdminUser[]>('GET', '/users'),
  giftCodes: () => adminRequest<GiftCode[]>('GET', '/gift-codes'),
  createGiftCode: (request: { amount_paise: number; max_uses: number; expires_at: string | null; note: string | null }) =>
    adminRequest<GiftCode & { code: string }>('POST', '/gift-codes', { json: request }),
  setGiftCodeDisabled: (id: string, disabled: boolean) => adminRequest<GiftCode>('PATCH', `/gift-codes/${id}`, { json: { disabled } }),

  updateUser: (id: string, change: { enabled?: boolean }) =>
    adminRequest<AdminUser>('PATCH', `/users/${id}`, { json: change }),
  relinkCandidates: () => adminRequest<RelinkCandidate[]>('GET', '/users/relink-candidates'),
  relinkUser: (fromUserId: string, intoUserId: string) =>
    adminRequest<AdminUser>('POST', '/users/relink', { json: { from_user_id: fromUserId, into_user_id: intoUserId } }),
  makeProjectOwner: (projectId: string, userId: string) =>
    adminRequest<AdminProject['members'][number]>('POST', `/projects/${projectId}/owners`, { json: { user_id: userId } }),

  projects: () => adminRequest<AdminProject[]>('GET', '/projects'),
  setProjectEnabled: (id: string, enabled: boolean) => adminRequest<AdminProject>('PATCH', `/projects/${id}`, { json: { enabled } }),
  adoptableOwners: () => adminRequest<AdoptableOwner[]>('GET', '/projects/adoptable-owners'),
  adoptOwner: (ownerId: string, name: string, userId: string) =>
    adminRequest<{ project_id: string; name: string }>('POST', '/projects/adopt-owner', { json: { owner_id: ownerId, name, user_id: userId } }),

  overview: () => adminRequest<Overview>('GET', '/overview'),

  nodes: () => adminRequest<AdminNode[]>('GET', '/nodes'),
  drainNode: (id: string) => adminRequest<void>('POST', `/nodes/${id}/drain`),
  resumeNode: (id: string) => adminRequest<void>('POST', `/nodes/${id}/resume`),
  removeNode: (id: string) => adminRequest<void>('POST', `/nodes/${id}/remove`),

  buckets: () => adminRequest<AdminBucket[]>('GET', '/buckets'),
  objects: (bucket: string, prefix: string, cursor?: string) => {
    const query: Record<string, string> = {};
    if (prefix) query['prefix'] = prefix;
    if (cursor) query['cursor'] = cursor;
    return adminRequest<ObjectPage>('GET', `/buckets/${seg(bucket)}/objects`, { query });
  },

  credentials: () => adminRequest<AdminCredential[]>('GET', '/credentials'),
  createCredential: (ownerId?: string) =>
    adminRequest<CreatedCredential>('POST', '/credentials', { json: ownerId ? { owner_id: ownerId } : {} }),
  setCredentialEnabled: (accessKey: string, enabled: boolean) =>
    adminRequest<AdminCredential>('PATCH', `/credentials/${accessKey}`, { json: { enabled } }),
  deleteCredential: (accessKey: string) => adminRequest<void>('DELETE', `/credentials/${accessKey}`),

  audit: (limit: number) => adminRequest<AuditEntry[]>('GET', '/audit', { query: { limit: String(limit) } }),
};
