import { useEffect, useState } from 'react';
import { Link, useParams, useSearchParams } from 'react-router-dom';
import { ArrowLeft, File, Folder } from 'lucide-react';
import { adminApi, AdminObject } from '@/api/admin';
import { Spinner } from '@/components/Spinner';
import { formatBytes, formatDateTime } from '@/lib/format';
import { AdminPageState, PageHeader, Table, secondaryButton } from './shared';

export function BucketContentsPage() {
  const { name = '' } = useParams();
  const [params, setParams] = useSearchParams();
  const prefix = params.get('prefix') ?? '';

  const [folders, setFolders] = useState<string[]>([]);
  const [objects, setObjects] = useState<AdminObject[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setFolders([]);
    setObjects([]);
    setCursor(null);
    setError(null);
    adminApi
      .objects(name, prefix)
      .then((page) => {
        if (cancelled) return;
        setFolders(page.folders);
        setObjects(page.objects);
        setCursor(page.next_cursor);
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(err);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [name, prefix]);

  async function loadMore() {
    if (!cursor) return;
    setLoadingMore(true);
    try {
      const page = await adminApi.objects(name, prefix, cursor);
      setFolders((prev) => [...new Set([...prev, ...page.folders])]);
      setObjects((prev) => [...prev, ...page.objects]);
      setCursor(page.next_cursor);
    } catch (err) {
      setError(err);
    } finally {
      setLoadingMore(false);
    }
  }

  const parent = prefix ? prefix.replace(/[^/]+\/$/, '') : null;
  const openPrefix = (p: string) => setParams(p ? { prefix: p } : {});
  const isEmpty = !loading && folders.length === 0 && objects.length === 0;

  return (
    <>
      <Link to="/admin/buckets" className="inline-flex items-center gap-1 text-sm text-gray-500 hover:text-gray-800 mb-3">
        <ArrowLeft className="w-4 h-4" /> All buckets
      </Link>
      <PageHeader title={name} description={prefix ? `In ${prefix}` : undefined} />
      <AdminPageState error={error} loading={loading} hasData={false}>
        <Table headers={['Name', 'Size', 'Last modified', 'ETag']}>
          {parent !== null && (
            <tr>
              <td colSpan={4} className="px-4 py-3">
                <button onClick={() => openPrefix(parent)} className="flex items-center gap-2 text-blue-600 hover:text-blue-700">
                  <Folder className="w-4 h-4" /> ..
                </button>
              </td>
            </tr>
          )}
          {folders.map((folder) => (
            <tr key={folder}>
              <td colSpan={4} className="px-4 py-3">
                <button onClick={() => openPrefix(folder)} className="flex items-center gap-2 text-blue-600 hover:text-blue-700">
                  <Folder className="w-4 h-4" /> {folder.slice(prefix.length)}
                </button>
              </td>
            </tr>
          ))}
          {objects.map((object) => (
            <tr key={object.key}>
              <td className="px-4 py-3 text-gray-900">
                <span className="flex items-center gap-2 break-all"><File className="w-4 h-4 text-gray-400 shrink-0" /> {object.key.slice(prefix.length)}</span>
              </td>
              <td className="px-4 py-3 tabular-nums text-gray-700 whitespace-nowrap">{formatBytes(object.size)}</td>
              <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDateTime(object.last_modified)}</td>
              <td className="px-4 py-3"><code className="text-xs text-gray-500">{object.etag}</code></td>
            </tr>
          ))}
          {isEmpty && (
            <tr><td colSpan={4} className="px-4 py-8 text-center text-gray-500">Nothing here.</td></tr>
          )}
        </Table>
        {cursor && (
          <div className="flex justify-center mt-4">
            <button onClick={() => void loadMore()} disabled={loadingMore} className={`${secondaryButton} flex items-center gap-2`}>
              {loadingMore && <Spinner size="sm" />} Load more
            </button>
          </div>
        )}
      </AdminPageState>
    </>
  );
}
