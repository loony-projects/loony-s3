import { useEffect, useState, useRef, DragEvent, ChangeEvent } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import {
  ArrowLeft, Upload, Trash2, Download, Link, Folder,
  File, Image, FileVideo, FileAudio, FileText, ChevronRight, RefreshCw,
} from 'lucide-react';
import {
  listObjects, deleteObject, presignGetUrl, uploadObject,
  ObjectSummary, ListBucketResult,
} from '@/api/objects';
import { useToast } from '@/context/ToastContext';
import { ApiError } from '@/api/client';
import { Spinner } from '@/components/Spinner';
import { EmptyState } from '@/components/EmptyState';

// ── Helpers ────────────────────────────────────────────────────────────────────

function formatBytes(bytes: number): string {
  if (bytes === 0) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / 1024 ** i).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

// The backend's ListObjectsV2 response (like real S3's) doesn't carry a per-object
// content type -- that's only known from a HeadObject/GetObject call, which listing
// every row here would mean one request per row. Inferring from the key's extension is
// what most S3 browser UIs do for the same reason.
function fileIconForKey(key: string) {
  const ext = key.split('.').pop()?.toLowerCase() ?? '';
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp'].includes(ext)) return <Image className="w-4 h-4 text-purple-500" />;
  if (['mp4', 'webm', 'mov', 'avi', 'mkv'].includes(ext)) return <FileVideo className="w-4 h-4 text-pink-500" />;
  if (['mp3', 'wav', 'ogg', 'flac', 'm4a'].includes(ext)) return <FileAudio className="w-4 h-4 text-yellow-500" />;
  if (['txt', 'md', 'csv', 'json', 'log', 'yaml', 'yml', 'toml'].includes(ext)) return <FileText className="w-4 h-4 text-blue-500" />;
  return <File className="w-4 h-4 text-gray-400" />;
}

// ── Upload progress overlay ────────────────────────────────────────────────────

interface UploadTask { name: string; pct: number; done: boolean; error?: string }

function UploadProgressPanel({ tasks }: { tasks: UploadTask[] }) {
  if (tasks.length === 0) return null;
  return (
    <div className="fixed bottom-4 left-4 w-72 bg-white border border-gray-200 rounded-xl shadow-lg p-4 space-y-2 z-40">
      <p className="text-xs font-semibold text-gray-500 uppercase tracking-wide mb-2">Uploading</p>
      {tasks.map((t, i) => (
        <div key={i}>
          <div className="flex justify-between text-xs text-gray-600 mb-1">
            <span className="truncate max-w-[180px]">{t.name}</span>
            <span>{t.error ? '✗' : t.done ? '✓' : `${t.pct}%`}</span>
          </div>
          <div className="h-1.5 bg-gray-100 rounded-full overflow-hidden">
            <div
              className={`h-full rounded-full transition-all ${t.error ? 'bg-red-400' : t.done ? 'bg-green-500' : 'bg-blue-500'}`}
              style={{ width: `${t.done || t.error ? 100 : t.pct}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}

// ── Breadcrumb ─────────────────────────────────────────────────────────────────

function Breadcrumb({ bucket, prefix, onNavigate }: {
  bucket: string;
  prefix: string;
  onNavigate: (p: string) => void;
}) {
  const parts = prefix.split('/').filter(Boolean);
  return (
    <nav className="flex items-center gap-1 text-sm flex-wrap">
      <button onClick={() => onNavigate('')} className="text-blue-600 hover:text-blue-700 font-medium">
        {bucket}
      </button>
      {parts.map((part, i) => {
        const target = parts.slice(0, i + 1).join('/') + '/';
        return (
          <span key={i} className="flex items-center gap-1">
            <ChevronRight className="w-3.5 h-3.5 text-gray-400" />
            <button
              onClick={() => onNavigate(target)}
              className={i === parts.length - 1
                ? 'text-gray-700 font-medium'
                : 'text-blue-600 hover:text-blue-700'}
            >
              {part}
            </button>
          </span>
        );
      })}
    </nav>
  );
}

// ── Main page ──────────────────────────────────────────────────────────────────

export function BucketPage() {
  const { name: bucket = '' } = useParams<{ name: string }>();
  const navigate = useNavigate();
  const toast = useToast();
  const fileInputRef = useRef<HTMLInputElement>(null);

  const [prefix, setPrefix] = useState('');
  const [result, setResult] = useState<ListBucketResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [dragging, setDragging] = useState(false);
  const [uploadTasks, setUploadTasks] = useState<UploadTask[]>([]);
  const [deletingKey, setDeletingKey] = useState<string | null>(null);
  const [presignKey, setPresignKey] = useState<string | null>(null);

  async function load(p = prefix) {
    setLoading(true);
    try {
      const res = await listObjects(bucket, p, '/');
      setResult(res);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to load objects');
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void load(prefix); }, [prefix]);

  function navigate_prefix(p: string) {
    setPrefix(p);
  }

  // ── Upload ──────────────────────────────────────────────────────────────────

  async function uploadFiles(files: File[]) {
    if (files.length === 0) return;

    const tasks: UploadTask[] = files.map((f) => ({ name: f.name, pct: 0, done: false }));
    setUploadTasks(tasks);

    await Promise.all(
      files.map(async (file, idx) => {
        const key = prefix + file.name;
        try {
          await uploadObject(bucket, key, file, (pct) => {
            setUploadTasks((prev) => prev.map((t, i) => i === idx ? { ...t, pct } : t));
          });
          setUploadTasks((prev) => prev.map((t, i) => i === idx ? { ...t, pct: 100, done: true } : t));
        } catch (err) {
          const msg = err instanceof ApiError ? err.message : 'Upload failed';
          setUploadTasks((prev) => prev.map((t, i) => i === idx ? { ...t, error: msg } : t));
          toast.error(`Failed to upload ${file.name}: ${msg}`);
        }
      }),
    );

    setTimeout(() => setUploadTasks([]), 2000);
    await load(prefix);
    toast.success(`${files.length} file${files.length > 1 ? 's' : ''} uploaded`);
  }

  function onFileChange(e: ChangeEvent<HTMLInputElement>) {
    const files = Array.from(e.target.files ?? []);
    e.target.value = '';
    void uploadFiles(files);
  }

  function onDrop(e: DragEvent) {
    e.preventDefault();
    setDragging(false);
    const files = Array.from(e.dataTransfer.files);
    void uploadFiles(files);
  }

  // ── Delete ──────────────────────────────────────────────────────────────────

  async function handleDelete(key: string) {
    if (!confirm(`Delete "${key}"?`)) return;
    setDeletingKey(key);
    try {
      await deleteObject(bucket, key);
      toast.success('Object deleted');
      await load(prefix);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Delete failed');
    } finally {
      setDeletingKey(null);
    }
  }

  // ── Download ────────────────────────────────────────────────────────────────
  // Presigned URLs are now computed entirely client-side (no server round trip) and
  // are already absolute, so there's no endpoint prefix to add.

  async function handleDownload(key: string) {
    try {
      const url = await presignGetUrl(bucket, key, 300);
      // The `download` attribute on an <a> is only honored by browsers for
      // same-origin URLs -- for a cross-origin one (the normal case here: the API
      // is deployed on a different origin than this app, vite.config.ts) browsers
      // silently ignore it and just navigate the tab to the raw resource instead,
      // replacing the app rather than downloading anything. Fetching the bytes into
      // a blob first sidesteps that: a blob: URL is always same-origin, so `download`
      // works regardless of where the API lives.
      const res = await fetch(url);
      if (!res.ok) throw new ApiError(res.status, 'Failed to download object');
      const blob = await res.blob();
      const objectUrl = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = objectUrl;
      link.download = key.split('/').pop() ?? key;
      link.click();
      URL.revokeObjectURL(objectUrl);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to download object');
    }
  }

  async function handleCopyLink(key: string) {
    setPresignKey(key);
    try {
      const url = await presignGetUrl(bucket, key, 3600);
      await navigator.clipboard.writeText(url);
      toast.success('Presigned URL copied (valid 1 hr)');
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to generate link');
    } finally {
      setPresignKey(null);
    }
  }

  // ── Objects in current prefix ───────────────────────────────────────────────

  const objects = result?.objects ?? [];
  const folders = result?.commonPrefixes ?? [];
  const isEmpty = objects.length === 0 && folders.length === 0;

  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-4">
          <button
            onClick={() => navigate('/')}
            className="text-gray-400 hover:text-gray-600 transition-colors"
          >
            <ArrowLeft className="w-5 h-5" />
          </button>
          <Breadcrumb bucket={bucket} prefix={prefix} onNavigate={navigate_prefix} />
        </div>

        <div className="flex items-center gap-2">
          <button
            onClick={() => void load(prefix)}
            className="p-2 text-gray-400 hover:text-gray-600 transition-colors rounded-lg hover:bg-gray-100"
            title="Refresh"
          >
            <RefreshCw className="w-4 h-4" />
          </button>
          <button
            onClick={() => fileInputRef.current?.click()}
            className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors"
          >
            <Upload className="w-4 h-4" />
            Upload
          </button>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            onChange={onFileChange}
          />
        </div>
      </div>

      {/* Drop zone wrapper */}
      <div
        onDragOver={(e) => { e.preventDefault(); setDragging(true); }}
        onDragLeave={() => setDragging(false)}
        onDrop={onDrop}
        className={`rounded-xl border-2 transition-all ${
          dragging
            ? 'border-blue-400 bg-blue-50'
            : 'border-transparent'
        }`}
      >
        {dragging && (
          <div className="flex items-center justify-center h-32 text-blue-500 font-medium">
            Drop files to upload
          </div>
        )}

        {!dragging && (
          <>
            {loading ? (
              <div className="flex justify-center py-20"><Spinner size="lg" /></div>
            ) : isEmpty ? (
              <EmptyState
                icon={<Folder className="w-16 h-16" />}
                title="No objects here"
                description="Upload files or drag and drop them here."
                action={
                  <button
                    onClick={() => fileInputRef.current?.click()}
                    className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors"
                  >
                    Upload files
                  </button>
                }
              />
            ) : (
              <div className="bg-white rounded-xl border border-gray-200 overflow-hidden">
                <table className="w-full text-sm">
                  <thead>
                    <tr className="border-b border-gray-100 bg-gray-50">
                      <th className="text-left px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide">Name</th>
                      <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide hidden sm:table-cell">Size</th>
                      <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide hidden md:table-cell">Modified</th>
                      <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide">Actions</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-gray-50">
                    {/* Virtual folders */}
                    {folders.map((folder) => {
                      const folderName = folder.slice(prefix.length).replace(/\/$/, '');
                      return (
                        <tr
                          key={folder}
                          className="hover:bg-gray-50 cursor-pointer transition-colors"
                          onClick={() => navigate_prefix(folder)}
                        >
                          <td className="px-4 py-3">
                            <div className="flex items-center gap-2.5">
                              <Folder className="w-4 h-4 text-amber-400 shrink-0" />
                              <span className="font-medium text-gray-800">{folderName}/</span>
                            </div>
                          </td>
                          <td className="px-4 py-3 text-right text-gray-400 hidden sm:table-cell">—</td>
                          <td className="px-4 py-3 text-right text-gray-400 hidden md:table-cell">—</td>
                          <td className="px-4 py-3 text-right text-gray-400">—</td>
                        </tr>
                      );
                    })}

                    {/* Objects */}
                    {objects.map((obj: ObjectSummary) => {
                      const displayName = obj.key.slice(prefix.length);
                      return (
                        <tr key={obj.key} className="hover:bg-gray-50 transition-colors group">
                          <td className="px-4 py-3">
                            <div className="flex items-center gap-2.5">
                              {fileIconForKey(obj.key)}
                              <span className="text-gray-800 truncate max-w-[240px] sm:max-w-none">
                                {displayName}
                              </span>
                            </div>
                          </td>
                          <td className="px-4 py-3 text-right text-gray-500 hidden sm:table-cell">
                            {formatBytes(obj.size)}
                          </td>
                          <td className="px-4 py-3 text-right text-gray-500 hidden md:table-cell">
                            {formatDate(obj.lastModified)}
                          </td>
                          <td className="px-4 py-3">
                            <div className="flex items-center justify-end gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                              <button
                                onClick={() => void handleDownload(obj.key)}
                                title="Download"
                                className="p-1.5 text-gray-400 hover:text-blue-600 hover:bg-blue-50 rounded transition-colors"
                              >
                                <Download className="w-4 h-4" />
                              </button>
                              <button
                                onClick={() => void handleCopyLink(obj.key)}
                                disabled={presignKey === obj.key}
                                title="Copy presigned URL"
                                className="p-1.5 text-gray-400 hover:text-green-600 hover:bg-green-50 rounded transition-colors"
                              >
                                {presignKey === obj.key
                                  ? <Spinner size="sm" />
                                  : <Link className="w-4 h-4" />}
                              </button>
                              <button
                                onClick={() => void handleDelete(obj.key)}
                                disabled={deletingKey === obj.key}
                                title="Delete"
                                className="p-1.5 text-gray-400 hover:text-red-600 hover:bg-red-50 rounded transition-colors"
                              >
                                {deletingKey === obj.key
                                  ? <Spinner size="sm" />
                                  : <Trash2 className="w-4 h-4" />}
                              </button>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>

                {result?.isTruncated && (
                  <div className="px-4 py-3 border-t border-gray-100 text-center">
                    <button
                      onClick={() => void listObjects(bucket, prefix, '/', result.nextContinuationToken)
                        .then((r) => setResult((prev) => prev
                          ? { ...r, objects: [...prev.objects, ...r.objects] }
                          : r))}
                      className="text-sm text-blue-600 hover:text-blue-700 font-medium"
                    >
                      Load more
                    </button>
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </div>

      <UploadProgressPanel tasks={uploadTasks} />
    </div>
  );
}
