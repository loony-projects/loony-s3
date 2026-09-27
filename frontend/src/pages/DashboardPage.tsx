import { useEffect, useMemo, useState, FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { Plus, Trash2, FolderOpen, Database } from 'lucide-react';
import { listBuckets, createBucket, deleteBucket, Bucket } from '@/api/buckets';
import { useToast } from '@/context/ToastContext';
import { ApiError } from '@/api/client';
import { Modal } from '@/components/Modal';
import { Spinner } from '@/components/Spinner';
import { EmptyState } from '@/components/EmptyState';

import { formatDate } from '@/lib/format';

type BucketSort = 'name-asc' | 'name-desc' | 'newest' | 'oldest';

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

const bucketComparators: Record<BucketSort, (a: Bucket, b: Bucket) => number> = {
  'name-asc': (a, b) => collator.compare(a.name, b.name),
  'name-desc': (a, b) => collator.compare(b.name, a.name),
  newest: (a, b) => Date.parse(b.creationDate) - Date.parse(a.creationDate),
  oldest: (a, b) => Date.parse(a.creationDate) - Date.parse(b.creationDate),
};

export function DashboardPage() {
  const [buckets, setBuckets] = useState<Bucket[]>([]);
  const [loading, setLoading] = useState(true);
  const [showCreate, setShowCreate] = useState(false);
  const [createName, setCreateName] = useState('');
  const [creating, setCreating] = useState(false);
  const [deletingName, setDeletingName] = useState<string | null>(null);
  const [sort, setSort] = useState<BucketSort>('name-asc');
  const toast = useToast();

  const sortedBuckets = useMemo(() => [...buckets].sort(bucketComparators[sort]), [buckets, sort]);
  const navigate = useNavigate();

  async function load() {
    try {
      setBuckets(await listBuckets());
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to load buckets');
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, []);

  async function handleCreate(e: FormEvent) {
    e.preventDefault();
    setCreating(true);
    try {
      const name = createName.trim();
      await createBucket(name);
      toast.success(`Bucket "${name}" created`);
      setShowCreate(false);
      setCreateName('');
      await load();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to create bucket');
    } finally {
      setCreating(false);
    }
  }

  async function handleDelete(bucket: Bucket) {
    if (!confirm(`Delete bucket "${bucket.name}"? This cannot be undone.`)) return;
    setDeletingName(bucket.name);
    try {
      await deleteBucket(bucket.name);
      setBuckets((prev) => prev.filter((b) => b.name !== bucket.name));
      toast.success(`Bucket "${bucket.name}" deleted`);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to delete bucket');
    } finally {
      setDeletingName(null);
    }
  }

  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-xl font-semibold text-gray-900">Buckets</h1>
          <p className="text-sm text-gray-500 mt-0.5">{buckets.length} bucket{buckets.length !== 1 ? 's' : ''}</p>
        </div>
        <div className="flex items-center gap-2">
          <select
            value={sort}
            onChange={(e) => setSort(e.target.value as BucketSort)}
            aria-label="Sort buckets"
            className="px-3 py-2 text-sm border border-gray-200 rounded-lg bg-white text-gray-700 focus:outline-none focus:ring-2 focus:ring-blue-500"
          >
            <option value="name-asc">Name (A–Z)</option>
            <option value="name-desc">Name (Z–A)</option>
            <option value="newest">Newest first</option>
            <option value="oldest">Oldest first</option>
          </select>
          <button
            onClick={() => setShowCreate(true)}
            className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors"
          >
            <Plus className="w-4 h-4" />
            Create bucket
          </button>
        </div>
      </div>

      {loading ? (
        <div className="flex justify-center py-20"><Spinner size="lg" /></div>
      ) : buckets.length === 0 ? (
        <EmptyState
          icon={<Database className="w-16 h-16" />}
          title="No buckets yet"
          description="Create your first bucket to start storing objects."
          action={
            <button
              onClick={() => setShowCreate(true)}
              className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors"
            >
              Create bucket
            </button>
          }
        />
      ) : (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {sortedBuckets.map((bucket) => (
            <div
              key={bucket.name}
              className="bg-white border border-gray-200 rounded-xl p-5 hover:border-blue-300 hover:shadow-sm transition-all group"
            >
              <div className="flex items-start justify-between mb-3">
                <div className="flex items-center gap-2.5">
                  <div className="w-9 h-9 bg-blue-50 rounded-lg flex items-center justify-center">
                    <FolderOpen className="w-5 h-5 text-blue-600" />
                  </div>
                  <div>
                    <h3 className="text-sm font-medium text-gray-900 truncate max-w-[160px]">
                      {bucket.name}
                    </h3>
                  </div>
                </div>
                <button
                  onClick={() => void handleDelete(bucket)}
                  disabled={deletingName === bucket.name}
                  className="opacity-0 group-hover:opacity-100 text-gray-300 hover:text-red-500 transition-all p-1 rounded"
                >
                  {deletingName === bucket.name ? <Spinner size="sm" /> : <Trash2 className="w-4 h-4" />}
                </button>
              </div>

              <div className="flex items-center justify-between">
                <p className="text-xs text-gray-400">Created {formatDate(bucket.creationDate)}</p>
                <button
                  onClick={() => navigate(`/buckets/${bucket.name}`)}
                  className="text-xs font-medium text-blue-600 hover:text-blue-700 transition-colors"
                >
                  Browse →
                </button>
              </div>
            </div>
          ))}
        </div>
      )}

      {showCreate && (
        <Modal
          title="Create bucket"
          onClose={() => { setShowCreate(false); setCreateName(''); }}
          footer={
            <>
              <button
                onClick={() => { setShowCreate(false); setCreateName(''); }}
                className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors"
              >
                Cancel
              </button>
              <button
                form="create-bucket-form"
                type="submit"
                disabled={creating || !createName.trim()}
                className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors"
              >
                {creating && <Spinner size="sm" />}
                Create
              </button>
            </>
          }
        >
          <form id="create-bucket-form" onSubmit={handleCreate} className="space-y-4">
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1.5">
                Bucket name
              </label>
              <input
                type="text"
                value={createName}
                onChange={(e) => setCreateName(e.target.value)}
                placeholder="my-bucket"
                required
                pattern="[a-z0-9][a-z0-9.\-]{1,61}[a-z0-9]"
                title="3–63 lowercase letters, numbers, hyphens, or dots"
                className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
              <p className="mt-1 text-xs text-gray-400">3–63 characters, lowercase letters, numbers, hyphens, dots</p>
            </div>
          </form>
        </Modal>
      )}
    </div>
  );
}
