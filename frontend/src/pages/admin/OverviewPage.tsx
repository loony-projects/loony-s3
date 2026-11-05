import { adminApi } from '@/api/admin';
import { formatBytes, formatDateTime } from '@/lib/format';
import { AdminPageState, PageHeader, useAdminData } from './shared';

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

function Stat({ label, value, hint }: { label: string; value: string | number; hint?: string }) {
  return (
    <div className="bg-white border border-gray-200 rounded-xl p-4">
      <p className="text-xs text-gray-500">{label}</p>
      <p className="text-2xl font-semibold text-gray-900 mt-1 tabular-nums">{value}</p>
      {hint && <p className="text-xs text-gray-400 mt-1">{hint}</p>}
    </div>
  );
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex justify-between gap-4 px-4 py-2.5 text-sm">
      <dt className="text-gray-500">{label}</dt>
      <dd className="text-gray-900 text-right break-all">{children}</dd>
    </div>
  );
}

export function OverviewPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.overview);

  return (
    <>
      <PageHeader title="Overview" description="Totals span every owner." onRefresh={() => void reload()} loading={loading} />
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && (
          <div className="space-y-6">
            <div className="grid grid-cols-2 lg:grid-cols-4 gap-4">
              <Stat label="Stored data" value={formatBytes(data.totals.bytes)} hint="Object sizes, before redundancy" />
              <Stat label="Objects" value={data.totals.objects.toLocaleString()} />
              <Stat label="Buckets" value={data.totals.buckets.toLocaleString()} />
              <Stat label="Owners" value={data.totals.owners.toLocaleString()} />
              <Stat
                label="Access keys"
                value={data.totals.credentials.toLocaleString()}
                hint={`${data.totals.enabled_credentials.toLocaleString()} enabled`}
              />
              <Stat label="Uploads in progress" value={data.totals.multipart_uploads.toLocaleString()} />
              <Stat label="Registered nodes" value={data.mode === 'standalone' ? '—' : data.totals.nodes.toLocaleString()} hint={data.mode === 'standalone' ? 'Standalone mode' : undefined} />
              <Stat label="Uptime" value={formatUptime(data.uptime_seconds)} hint={`Since ${formatDateTime(data.started_at)}`} />
            </div>

            <div className="grid md:grid-cols-2 gap-4">
              <section className="bg-white border border-gray-200 rounded-xl">
                <h3 className="px-4 pt-4 pb-2 text-sm font-medium text-gray-900">This server</h3>
                <dl className="divide-y divide-gray-100">
                  <Fact label="Mode">{data.mode}</Fact>
                  {data.cluster_id && <Fact label="Cluster">{data.cluster_id}</Fact>}
                  <Fact label="Node ID"><code className="text-xs">{data.node_id}</code></Fact>
                  <Fact label="Region">{data.region}</Fact>
                  <Fact label="Version">{data.version}</Fact>
                </dl>
              </section>
              <section className="bg-white border border-gray-200 rounded-xl">
                <h3 className="px-4 pt-4 pb-2 text-sm font-medium text-gray-900">Metadata group</h3>
                <dl className="divide-y divide-gray-100">
                  <Fact label="This node's role">{data.metadata.state}</Fact>
                  <Fact label="Leader">
                    {data.metadata.leader ? <code className="text-xs">{data.metadata.leader}</code> : 'None'}
                  </Fact>
                  <Fact label="Term">{data.metadata.term}</Fact>
                  <Fact label="Voters / learners">{data.metadata.voters} / {data.metadata.learners}</Fact>
                </dl>
              </section>
            </div>
          </div>
        )}
      </AdminPageState>
    </>
  );
}
