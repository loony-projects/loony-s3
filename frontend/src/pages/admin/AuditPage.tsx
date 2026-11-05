import { useState } from 'react';
import { ScrollText } from 'lucide-react';
import { adminApi, AuditEntry } from '@/api/admin';
import { EmptyState } from '@/components/EmptyState';
import { formatDateTime } from '@/lib/format';
import { AdminPageState, Badge, PageHeader, Table, useAdminData } from './shared';

const outcomeTone: Record<AuditEntry['outcome'], 'green' | 'amber' | 'red'> = { ok: 'green', denied: 'amber', failed: 'red' };

const LIMITS = [100, 200, 500, 1000];

export function AuditPage() {
  const [limit, setLimit] = useState(200);
  const { data, error, loading, reload } = useAdminData(() => adminApi.audit(limit), [limit]);

  return (
    <>
      <PageHeader
        title="Audit log"
        description="Admin changes, bucket browsing and rejected admin requests on this node, newest first. Uploads and downloads through the LS3 API aren't recorded here."
        onRefresh={() => void reload()}
        loading={loading}
      >
        <select
          value={limit}
          onChange={(e) => setLimit(Number(e.target.value))}
          aria-label="Number of entries to show"
          className="px-3 py-2 text-sm border border-gray-200 rounded-lg bg-white text-gray-700 focus:outline-none focus:ring-2 focus:ring-blue-500"
        >
          {LIMITS.map((n) => <option key={n} value={n}>Last {n}</option>)}
        </select>
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState icon={<ScrollText className="w-16 h-16" />} title="Nothing recorded yet" description="Admin actions will appear here." />
        ) : (
          data && (
            <Table headers={['Time', 'Key', 'Action', 'Target', 'Outcome', 'Detail']}>
              {data.map((entry, i) => (
                <tr key={`${entry.at}-${i}`}>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDateTime(entry.at)}</td>
                  <td className="px-4 py-3"><code className="text-xs text-gray-700">{entry.actor}</code></td>
                  <td className="px-4 py-3 text-gray-900 whitespace-nowrap">{entry.action}</td>
                  <td className="px-4 py-3 text-gray-700 min-w-64 break-all"><code className="text-xs">{entry.target}</code></td>
                  <td className="px-4 py-3"><Badge tone={outcomeTone[entry.outcome]}>{entry.outcome}</Badge></td>
                  <td className="px-4 py-3 text-gray-500 min-w-48">{entry.detail}</td>
                </tr>
              ))}
            </Table>
          )
        )}
      </AdminPageState>
    </>
  );
}
