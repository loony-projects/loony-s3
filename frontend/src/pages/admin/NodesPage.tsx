import { Server } from 'lucide-react';
import { adminApi, AdminNode, NodeState } from '@/api/admin';
import { EmptyState } from '@/components/EmptyState';
import { formatDateTime } from '@/lib/format';
import { AdminPageState, Badge, PageHeader, ShortId, Table, dangerButton, secondaryButton, useAdminAction, useAdminData } from './shared';

const stateTone: Record<NodeState, 'green' | 'blue' | 'amber' | 'red' | 'gray'> = {
  Healthy: 'green',
  Joining: 'blue',
  Suspect: 'amber',
  Draining: 'amber',
  Offline: 'red',
  Removed: 'gray',
};

function removeBlockedReason(node: AdminNode): string | undefined {
  if (node.state !== 'Draining') return 'Drain this node first';
  if (node.shards > 0) return `Still holds ${node.shards} shard(s), and shards can't be migrated yet`;
  return undefined;
}

export function NodesPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.nodes);
  const { busy, run } = useAdminAction(reload);

  function drain(node: AdminNode) {
    const ok = confirm(
      `Drain node ${node.advertised_address}?\n\nIt stops receiving new data. Data already on it stays there and stays readable: nothing is migrated off it.`,
    );
    if (ok) void run(node.node_id, () => adminApi.drainNode(node.node_id), 'Node is draining');
  }

  function remove(node: AdminNode) {
    const ok = confirm(
      `Remove node ${node.advertised_address} from the cluster?\n\nThis is permanent: it can't rejoin with this identity.`,
    );
    if (ok) void run(node.node_id, () => adminApi.removeNode(node.node_id), 'Node removed');
  }

  return (
    <>
      <PageHeader
        title="Nodes"
        description="Draining stops new data going to a node, but doesn't move existing data. A node can only be removed once it holds no shards."
        onRefresh={() => void reload()}
        loading={loading}
      />
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState
            icon={<Server className="w-16 h-16" />}
            title="No cluster nodes"
            description="This server is running in standalone mode, so there are no nodes to manage."
          />
        ) : (
          data && (
            <Table headers={['Node', 'Address', 'State', 'Shards', 'Volumes', 'Generation', 'Last change', '']}>
              {data.map((node) => (
                <tr key={node.node_id}>
                  <td className="px-4 py-3">
                    <div className="flex items-center gap-2">
                      <ShortId value={node.node_id} />
                      {node.is_leader && <Badge tone="blue">Leader</Badge>}
                      {node.is_self && <Badge tone="gray">This node</Badge>}
                    </div>
                  </td>
                  <td className="px-4 py-3 text-gray-700 font-mono text-xs">{node.advertised_address}</td>
                  <td className="px-4 py-3"><Badge tone={stateTone[node.state]}>{node.state}</Badge></td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{node.shards.toLocaleString()}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{node.volumes}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{node.generation}</td>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDateTime(node.last_seen)}</td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-2">
                      {node.is_voter ? (
                        <span className="text-xs text-gray-400" title="Voter changes aren't supported yet">Metadata voter</span>
                      ) : node.state === 'Removed' ? null : (
                        <>
                          {node.state === 'Draining' ? (
                            <button
                              className={secondaryButton}
                              disabled={busy !== null}
                              onClick={() => void run(node.node_id, () => adminApi.resumeNode(node.node_id), 'Node resumed')}
                            >
                              Resume
                            </button>
                          ) : (
                            <button className={secondaryButton} disabled={busy !== null} onClick={() => drain(node)}>
                              Drain
                            </button>
                          )}
                          <button
                            className={dangerButton}
                            disabled={busy !== null || removeBlockedReason(node) !== undefined}
                            title={removeBlockedReason(node)}
                            onClick={() => remove(node)}
                          >
                            Remove
                          </button>
                        </>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </Table>
          )
        )}
      </AdminPageState>
    </>
  );
}
