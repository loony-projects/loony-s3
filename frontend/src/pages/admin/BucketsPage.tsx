import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { FolderOpen } from 'lucide-react';
import { adminApi } from '@/api/admin';
import { EmptyState } from '@/components/EmptyState';
import { formatBytes, formatDate } from '@/lib/format';
import { AdminPageState, PageHeader, ShortId, Table, useAdminData } from './shared';

export function BucketsPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.buckets);
  const [filter, setFilter] = useState('');

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (data ?? []).filter((b) => b.name.includes(needle) || b.owner_id.includes(needle));
  }, [data, filter]);

  return (
    <>
      <PageHeader
        title="All buckets"
        description="Every owner's buckets. Browsing is read-only, shows names and sizes rather than contents, and is recorded in the audit log."
        onRefresh={() => void reload()}
        loading={loading}
      >
        <input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter by name or owner"
          aria-label="Filter buckets by name or owner"
          className="px-3 py-2 text-sm border border-gray-200 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500"
        />
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState icon={<FolderOpen className="w-16 h-16" />} title="No buckets yet" description="No owner has created a bucket." />
        ) : (
          data && (
            <Table headers={['Bucket', 'Owner', 'Objects', 'Size', 'Region', 'Created']}>
              {visible.map((bucket) => (
                <tr key={bucket.name}>
                  <td className="px-4 py-3">
                    <Link to={`/admin/buckets/${encodeURIComponent(bucket.name)}`} className="font-medium text-blue-600 hover:text-blue-700">
                      {bucket.name}
                    </Link>
                  </td>
                  <td className="px-4 py-3"><ShortId value={bucket.owner_id} /></td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{bucket.object_count.toLocaleString()}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700 whitespace-nowrap">{formatBytes(bucket.total_bytes)}</td>
                  <td className="px-4 py-3 text-gray-700">{bucket.region}</td>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(bucket.created_at)}</td>
                </tr>
              ))}
              {visible.length === 0 && (
                <tr><td colSpan={6} className="px-4 py-8 text-center text-gray-500">No buckets match that filter.</td></tr>
              )}
            </Table>
          )
        )}
      </AdminPageState>
    </>
  );
}
