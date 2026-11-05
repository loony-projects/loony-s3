import { ApiError, getCredentials, signedFetch } from './client';

/** The admin API only listens on loopback, separately from the LS3 endpoint used to sign in. */
export const ADMIN_ENDPOINT = (import.meta.env['VITE_ADMIN_API_URL'] as string | undefined) || 'http://127.0.0.1:7253';

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

export const adminApi = {
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
