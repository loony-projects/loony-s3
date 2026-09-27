import { useEffect, useMemo, useRef, useState, DragEvent, ChangeEvent } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import {
  ArrowLeft, Upload, Trash2, Download, Link, Folder, FolderUp, Info, Search, X,
  File, Image, FileVideo, FileAudio, FileText, ChevronRight, RefreshCw, ArrowUp, ArrowDown, ArrowUpDown,
} from 'lucide-react';
import {
  listObjects, listAllKeys, deleteObjects, presignObjectUrl, uploadFile, isAbort,
  type ObjectSummary, type ListBucketResult,
} from '@/api/objects';
import { useToast } from '@/context/ToastContext';
import { ApiError } from '@/api/client';
import { Spinner } from '@/components/Spinner';
import { EmptyState } from '@/components/EmptyState';
import { UploadPanel, type UploadTask } from '@/components/UploadPanel';
import { ObjectDetailsDrawer } from '@/components/ObjectDetailsDrawer';
import { ShareLinkModal, type ShareMode } from '@/components/ShareLinkModal';
import { formatBytes, formatDate, fileExtension } from '@/lib/format';
import { runPool } from '@/lib/pool';

const FILE_CONCURRENCY = 3;
const SEARCH_DEBOUNCE_MS = 300;

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;

// The ListObjectsV2 response doesn't carry a per-object content type -- that's only
// known from a HeadObject/GetObject call, which listing every row here would mean one
// request per row. Inferring from the key's extension is what most object-storage
// browser UIs do for the same reason.
function fileIconForKey(key: string) {
  const ext = fileExtension(key);
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp'].includes(ext)) return <Image className="w-4 h-4 text-purple-500 shrink-0" />;
  if (['mp4', 'webm', 'mov', 'avi', 'mkv'].includes(ext)) return <FileVideo className="w-4 h-4 text-pink-500 shrink-0" />;
  if (['mp3', 'wav', 'ogg', 'flac', 'm4a'].includes(ext)) return <FileAudio className="w-4 h-4 text-yellow-500 shrink-0" />;
  if (['txt', 'md', 'csv', 'json', 'log', 'yaml', 'yml', 'toml'].includes(ext)) return <FileText className="w-4 h-4 text-blue-500 shrink-0" />;
  return <File className="w-4 h-4 text-gray-400 shrink-0" />;
}

// ── Folder drag-and-drop ───────────────────────────────────────────────────────

interface PendingFile { file: File; relPath: string }

/** Expands dropped entries (files and whole directories) into files with their paths. */
async function readDroppedEntries(entries: FileSystemEntry[]): Promise<PendingFile[]> {
  const out: PendingFile[] = [];
  async function walk(entry: FileSystemEntry, dir: string): Promise<void> {
    if (entry.isFile) {
      const file = await new Promise<File>((resolve, reject) => (entry as FileSystemFileEntry).file(resolve, reject));
      out.push({ file, relPath: dir + entry.name });
    } else if (entry.isDirectory) {
      const reader = (entry as FileSystemDirectoryEntry).createReader();
      // readEntries hands results back in batches; an empty batch means done.
      for (;;) {
        const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => reader.readEntries(resolve, reject));
        if (batch.length === 0) break;
        for (const child of batch) await walk(child, `${dir}${entry.name}/`);
      }
    }
  }
  for (const entry of entries) await walk(entry, '');
  return out;
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
              className={i === parts.length - 1 ? 'text-gray-700 font-medium' : 'text-blue-600 hover:text-blue-700'}
            >
              {part}
            </button>
          </span>
        );
      })}
    </nav>
  );
}

// ── Sorting ────────────────────────────────────────────────────────────────────

type SortField = 'name' | 'size' | 'modified';
interface Sort { field: SortField; dir: 'asc' | 'desc' }

function SortHeader({ label, field, sort, onSort, className = '' }: {
  label: string;
  field: SortField;
  sort: Sort;
  onSort: (f: SortField) => void;
  className?: string;
}) {
  const active = sort.field === field;
  const Icon = !active ? ArrowUpDown : sort.dir === 'asc' ? ArrowUp : ArrowDown;
  return (
    <th className={`px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide ${className}`}>
      <button onClick={() => onSort(field)} className="inline-flex items-center gap-1 hover:text-gray-700 uppercase">
        {label}
        <Icon className={`w-3 h-3 ${active ? '' : 'opacity-40'}`} />
      </button>
    </th>
  );
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

// ── Main page ──────────────────────────────────────────────────────────────────

export function BucketPage() {
  const { name: bucket = '' } = useParams<{ name: string }>();
  const navigate = useNavigate();
  const toast = useToast();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);

  const [prefix, setPrefix] = useState('');
  const [searchInput, setSearchInput] = useState('');
  const [search, setSearch] = useState('');
  const [result, setResult] = useState<ListBucketResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [sort, setSort] = useState<Sort>({ field: 'name', dir: 'asc' });
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [dragging, setDragging] = useState(false);
  const [uploadTasks, setUploadTasks] = useState<UploadTask[]>([]);
  const [bulk, setBulk] = useState<{ label: string; done: number; total: number } | null>(null);
  const [details, setDetails] = useState<ObjectSummary | null>(null);
  const [share, setShare] = useState<{ mode: ShareMode; key: string } | null>(null);

  const uploadControllers = useRef(new Map<number, AbortController>());
  const nextUploadId = useRef(1);
  const loadSeq = useRef(0);

  // `prefix + search` is a server-side prefix filter within the current folder.
  const listPrefix = prefix + search;

  async function load() {
    const seq = ++loadSeq.current;
    setLoading(true);
    try {
      const res = await listObjects(bucket, listPrefix, '/');
      if (seq === loadSeq.current) setResult(res);
    } catch (err) {
      if (seq === loadSeq.current) toast.error(err instanceof ApiError ? err.message : 'Failed to load objects');
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }
  // Async completions (uploads, deletes) must reload whatever folder is showing *then*.
  const reloadRef = useRef(load);
  reloadRef.current = load;

  useEffect(() => {
    setSelected(new Set());
    void load();
  }, [bucket, listPrefix]);

  useEffect(() => {
    const t = setTimeout(() => setSearch(searchInput), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [searchInput]);

  useEffect(() => {
    folderInputRef.current?.setAttribute('webkitdirectory', '');
  }, []);

  function navigatePrefix(p: string) {
    setPrefix(p);
    setSearchInput('');
    setSearch('');
  }

  async function loadMore() {
    const token = result?.nextContinuationToken;
    if (!token) return;
    const seq = loadSeq.current;
    setLoadingMore(true);
    try {
      const page = await listObjects(bucket, listPrefix, '/', token);
      if (seq !== loadSeq.current) return;
      setResult((prev) => prev
        ? {
            objects: [...prev.objects, ...page.objects],
            commonPrefixes: Array.from(new Set([...prev.commonPrefixes, ...page.commonPrefixes])),
            isTruncated: page.isTruncated,
            nextContinuationToken: page.nextContinuationToken,
          }
        : page);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to load more objects');
    } finally {
      setLoadingMore(false);
    }
  }

  // ── Sorted view ─────────────────────────────────────────────────────────────

  const folders = useMemo(() => {
    const list = [...(result?.commonPrefixes ?? [])].sort(collator.compare);
    return sort.field === 'name' && sort.dir === 'desc' ? list.reverse() : list;
  }, [result, sort]);

  const objects = useMemo(() => {
    const cmp: Record<SortField, (a: ObjectSummary, b: ObjectSummary) => number> = {
      name: (a, b) => collator.compare(a.key, b.key),
      size: (a, b) => a.size - b.size,
      modified: (a, b) => Date.parse(a.lastModified) - Date.parse(b.lastModified),
    };
    const list = [...(result?.objects ?? [])];
    list.sort((a, b) => (sort.dir === 'asc' ? 1 : -1) * cmp[sort.field](a, b));
    return list;
  }, [result, sort]);

  function onSort(field: SortField) {
    setSort((prev) => prev.field === field
      ? { field, dir: prev.dir === 'asc' ? 'desc' : 'asc' }
      : { field, dir: field === 'name' ? 'asc' : 'desc' });
  }

  // ── Selection ───────────────────────────────────────────────────────────────

  const visibleIds = useMemo(() => [...folders, ...objects.map((o) => o.key)], [folders, objects]);
  const allSelected = visibleIds.length > 0 && visibleIds.every((id) => selected.has(id));

  function toggleSelected(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  }

  function toggleAll() {
    setSelected(allSelected ? new Set() : new Set(visibleIds));
  }

  // ── Upload ──────────────────────────────────────────────────────────────────

  function updateTask(id: number, patch: Partial<UploadTask>) {
    setUploadTasks((prev) => prev.map((t) => (t.id === id ? { ...t, ...patch } : t)));
  }

  function uploadFiles(files: PendingFile[]) {
    if (files.length === 0) return;
    const base = prefix;
    const batch = files.map((f) => ({ ...f, id: nextUploadId.current++ }));
    for (const b of batch) uploadControllers.current.set(b.id, new AbortController());
    setUploadTasks((prev) => [
      // Starting a new batch clears finished rows from earlier ones.
      ...prev.filter((t) => t.status === 'queued' || t.status === 'uploading'),
      ...batch.map((b): UploadTask => ({ id: b.id, name: b.relPath, loaded: 0, total: b.file.size, status: 'queued' })),
    ]);

    let succeeded = 0;
    let failed = 0;
    let cancelled = 0;
    void runPool(batch, FILE_CONCURRENCY, async (b) => {
      const controller = uploadControllers.current.get(b.id);
      if (!controller || controller.signal.aborted) {
        cancelled++;
        updateTask(b.id, { status: 'cancelled' });
        return;
      }
      updateTask(b.id, { status: 'uploading' });
      try {
        await uploadFile(bucket, base + b.relPath, b.file, {
          signal: controller.signal,
          onProgress: (loaded) => updateTask(b.id, { loaded }),
        });
        succeeded++;
        updateTask(b.id, { status: 'done', loaded: b.file.size });
      } catch (err) {
        if (isAbort(err)) {
          cancelled++;
          updateTask(b.id, { status: 'cancelled' });
        } else {
          failed++;
          updateTask(b.id, { status: 'error', error: err instanceof ApiError ? err.message : 'Upload failed' });
        }
      } finally {
        uploadControllers.current.delete(b.id);
      }
    }).then(async () => {
      if (succeeded > 0) toast.success(`${plural(succeeded, 'file')} uploaded`);
      if (failed > 0) toast.error(`${plural(failed, 'file')} failed to upload`);
      if (cancelled > 0 && succeeded === 0 && failed === 0) toast.info('Upload cancelled');
      await reloadRef.current();
    });
  }

  function cancelUpload(id: number) {
    uploadControllers.current.get(id)?.abort();
    setUploadTasks((prev) => prev.map((t) => (t.id === id && t.status === 'queued' ? { ...t, status: 'cancelled' } : t)));
  }

  function cancelAllUploads() {
    for (const c of uploadControllers.current.values()) c.abort();
    setUploadTasks((prev) => prev.map((t) => (t.status === 'queued' ? { ...t, status: 'cancelled' } : t)));
  }

  function onFileChange(e: ChangeEvent<HTMLInputElement>) {
    // Folder picks carry each file's path inside the chosen folder; plain picks don't.
    const files = Array.from(e.target.files ?? []).map((file) => ({ file, relPath: file.webkitRelativePath || file.name }));
    e.target.value = '';
    uploadFiles(files);
  }

  function onDrop(e: DragEvent) {
    e.preventDefault();
    setDragging(false);
    // Entries must be taken synchronously -- the DataTransfer is emptied once this
    // handler returns.
    const entries = Array.from(e.dataTransfer.items ?? [])
      .map((item) => item.webkitGetAsEntry?.() ?? null)
      .filter((entry): entry is FileSystemEntry => entry !== null);
    if (entries.length > 0) {
      readDroppedEntries(entries)
        .then(uploadFiles)
        .catch(() => toast.error('Could not read the dropped files'));
    } else {
      uploadFiles(Array.from(e.dataTransfer.files).map((file) => ({ file, relPath: file.name })));
    }
  }

  // ── Delete (single, bulk, and whole folders) ────────────────────────────────

  async function deleteItems(ids: string[]) {
    const folderIds = ids.filter((id) => id.endsWith('/'));
    let keys = ids.filter((id) => !id.endsWith('/'));

    if (folderIds.length > 0) {
      setBulk({ label: 'Counting objects…', done: 0, total: 0 });
      try {
        for (const folder of folderIds) keys = keys.concat(await listAllKeys(bucket, folder));
      } catch (err) {
        toast.error(err instanceof ApiError ? err.message : 'Failed to list folder contents');
        setBulk(null);
        return;
      }
      setBulk(null);
    }
    keys = Array.from(new Set(keys));
    if (keys.length === 0) {
      toast.info('Nothing to delete');
      return;
    }

    const message = folderIds.length === 0 && keys.length === 1
      ? `Delete "${keys[0]}"?`
      : `Delete ${plural(keys.length, 'object')}${folderIds.length > 0 ? `, including everything inside ${plural(folderIds.length, 'folder')}` : ''}? This cannot be undone.`;
    if (!confirm(message)) return;

    setBulk({ label: 'Deleting', done: 0, total: keys.length });
    const res = await deleteObjects(bucket, keys, (done, total) => setBulk({ label: 'Deleting', done, total }));
    setBulk(null);
    setSelected(new Set());
    if (details && keys.includes(details.key) && !res.failed.some((f) => f.key === details.key)) setDetails(null);

    if (res.deleted > 0) toast.success(`Deleted ${plural(res.deleted, 'object')}`);
    if (res.failed.length > 0) toast.error(`${plural(res.failed.length, 'object')} could not be deleted: ${res.failed[0].message}`);
    await reloadRef.current();
  }

  // ── Download ────────────────────────────────────────────────────────────────

  async function handleDownload(key: string) {
    try {
      const url = await presignObjectUrl(bucket, key, 'GET', 300);
      // The `download` attribute on an <a> is only honored for same-origin URLs --
      // for a cross-origin one (the normal case here: the API is on a different origin
      // than this app) browsers ignore it and navigate the tab to the raw resource,
      // replacing the app. A blob: URL is always same-origin, so fetch into one first.
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

  // ── Render ──────────────────────────────────────────────────────────────────

  const isEmpty = objects.length === 0 && folders.length === 0;
  const busy = bulk !== null;
  const iconButton = 'p-1.5 text-gray-400 rounded transition-colors disabled:opacity-40';

  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
      {/* Header */}
      <div className="flex flex-wrap items-center justify-between gap-3 mb-4">
        <div className="flex items-center gap-4">
          <button onClick={() => navigate('/')} title="All buckets" className="text-gray-400 hover:text-gray-600">
            <ArrowLeft className="w-5 h-5" />
          </button>
          <Breadcrumb bucket={bucket} prefix={prefix} onNavigate={navigatePrefix} />
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <button
            onClick={() => void load()}
            className="p-2 text-gray-400 hover:text-gray-600 rounded-lg hover:bg-gray-100"
            title="Refresh"
          >
            <RefreshCw className="w-4 h-4" />
          </button>
          <button
            onClick={() => setShare({ mode: 'upload', key: prefix })}
            className="flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 border border-gray-200 rounded-lg hover:bg-gray-50"
            title="Create a link someone else can upload a file with"
          >
            <Link className="w-4 h-4" />
            Upload link
          </button>
          <button
            onClick={() => folderInputRef.current?.click()}
            className="flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 border border-gray-200 rounded-lg hover:bg-gray-50"
          >
            <FolderUp className="w-4 h-4" />
            Upload folder
          </button>
          <button
            onClick={() => fileInputRef.current?.click()}
            className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg"
          >
            <Upload className="w-4 h-4" />
            Upload files
          </button>
          <input ref={fileInputRef} type="file" multiple className="hidden" onChange={onFileChange} />
          <input ref={folderInputRef} type="file" multiple className="hidden" onChange={onFileChange} />
        </div>
      </div>

      {/* Search + selection bar */}
      <div className="flex flex-wrap items-center gap-3 mb-4">
        <div className="relative flex-1 min-w-[200px] max-w-md">
          <Search className="w-4 h-4 text-gray-400 absolute left-3 top-1/2 -translate-y-1/2" />
          <input
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
            placeholder={prefix ? `Search in ${prefix}` : 'Search by name prefix'}
            className="w-full pl-9 pr-8 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
          />
          {searchInput && (
            <button
              onClick={() => { setSearchInput(''); setSearch(''); }}
              title="Clear search"
              className="absolute right-2 top-1/2 -translate-y-1/2 text-gray-400 hover:text-gray-600"
            >
              <X className="w-4 h-4" />
            </button>
          )}
        </div>

        {bulk ? (
          <div className="flex items-center gap-2 text-sm text-gray-600">
            <Spinner size="sm" />
            {bulk.total > 0 ? `${bulk.label} ${bulk.done}/${bulk.total}…` : bulk.label}
          </div>
        ) : selected.size > 0 && (
          <div className="flex items-center gap-3 text-sm">
            <span className="text-gray-600">{selected.size} selected</span>
            <button
              onClick={() => void deleteItems([...selected])}
              className="flex items-center gap-1.5 px-3 py-1.5 text-red-600 border border-red-200 rounded-lg hover:bg-red-50"
            >
              <Trash2 className="w-4 h-4" /> Delete
            </button>
            <button onClick={() => setSelected(new Set())} className="text-gray-500 hover:text-gray-700">
              Clear
            </button>
          </div>
        )}
      </div>

      {/* Drop zone wrapper */}
      <div
        onDragOver={(e) => { e.preventDefault(); setDragging(true); }}
        onDragLeave={(e) => { if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDragging(false); }}
        onDrop={onDrop}
        className={`rounded-xl border-2 transition-all ${dragging ? 'border-blue-400 bg-blue-50' : 'border-transparent'}`}
      >
        {dragging ? (
          <div className="flex items-center justify-center h-32 text-blue-500 font-medium">
            Drop files or folders to upload{prefix ? ` into ${prefix}` : ''}
          </div>
        ) : loading ? (
          <div className="flex justify-center py-20"><Spinner size="lg" /></div>
        ) : isEmpty ? (
          search ? (
            <EmptyState
              icon={<Search className="w-16 h-16" />}
              title="No matches"
              description={`Nothing in this folder starts with "${search}".`}
            />
          ) : (
            <EmptyState
              icon={<Folder className="w-16 h-16" />}
              title="No objects here"
              description="Upload files or folders, or drag and drop them here."
              action={
                <button
                  onClick={() => fileInputRef.current?.click()}
                  className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg"
                >
                  Upload files
                </button>
              }
            />
          )
        ) : (
          <div className="bg-white rounded-xl border border-gray-200 overflow-hidden">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-gray-100 bg-gray-50">
                  <th className="w-10 pl-4 py-3">
                    <input type="checkbox" checked={allSelected} onChange={toggleAll} disabled={busy} aria-label="Select all" />
                  </th>
                  <SortHeader label="Name" field="name" sort={sort} onSort={onSort} className="text-left" />
                  <SortHeader label="Size" field="size" sort={sort} onSort={onSort} className="text-right hidden sm:table-cell" />
                  <SortHeader label="Modified" field="modified" sort={sort} onSort={onSort} className="text-right hidden md:table-cell" />
                  <th className="text-right px-4 py-3 text-xs font-medium text-gray-500 uppercase tracking-wide">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-50">
                {folders.map((folder) => {
                  const folderName = folder.slice(prefix.length).replace(/\/$/, '');
                  return (
                    <tr key={folder} className="hover:bg-gray-50 transition-colors group">
                      <td className="pl-4 py-3">
                        <input
                          type="checkbox"
                          checked={selected.has(folder)}
                          onChange={() => toggleSelected(folder)}
                          disabled={busy}
                          aria-label={`Select ${folderName}`}
                        />
                      </td>
                      <td className="px-4 py-3">
                        <button onClick={() => navigatePrefix(folder)} className="flex items-center gap-2.5 text-left">
                          <Folder className="w-4 h-4 text-amber-400 shrink-0" />
                          <span className="font-medium text-gray-800 hover:text-blue-600">{folderName}/</span>
                        </button>
                      </td>
                      <td className="px-4 py-3 text-right text-gray-400 hidden sm:table-cell">—</td>
                      <td className="px-4 py-3 text-right text-gray-400 hidden md:table-cell">—</td>
                      <td className="px-4 py-3">
                        <div className="flex items-center justify-end gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
                          <button
                            onClick={() => void deleteItems([folder])}
                            disabled={busy}
                            title="Delete folder and everything in it"
                            className={`${iconButton} hover:text-red-600 hover:bg-red-50`}
                          >
                            <Trash2 className="w-4 h-4" />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}

                {objects.map((obj) => {
                  const displayName = obj.key.slice(prefix.length);
                  return (
                    <tr key={obj.key} className="hover:bg-gray-50 transition-colors group">
                      <td className="pl-4 py-3">
                        <input
                          type="checkbox"
                          checked={selected.has(obj.key)}
                          onChange={() => toggleSelected(obj.key)}
                          disabled={busy}
                          aria-label={`Select ${displayName}`}
                        />
                      </td>
                      <td className="px-4 py-3">
                        <button onClick={() => setDetails(obj)} className="flex items-center gap-2.5 text-left min-w-0">
                          {fileIconForKey(obj.key)}
                          <span className="text-gray-800 hover:text-blue-600 truncate max-w-[240px] sm:max-w-none">
                            {displayName}
                          </span>
                        </button>
                      </td>
                      <td className="px-4 py-3 text-right text-gray-500 hidden sm:table-cell">{formatBytes(obj.size)}</td>
                      <td className="px-4 py-3 text-right text-gray-500 hidden md:table-cell">{formatDate(obj.lastModified)}</td>
                      <td className="px-4 py-3">
                        <div className="flex items-center justify-end gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
                          <button onClick={() => setDetails(obj)} title="Details and preview" className={`${iconButton} hover:text-gray-700 hover:bg-gray-100`}>
                            <Info className="w-4 h-4" />
                          </button>
                          <button onClick={() => void handleDownload(obj.key)} title="Download" className={`${iconButton} hover:text-blue-600 hover:bg-blue-50`}>
                            <Download className="w-4 h-4" />
                          </button>
                          <button onClick={() => setShare({ mode: 'download', key: obj.key })} title="Share link" className={`${iconButton} hover:text-green-600 hover:bg-green-50`}>
                            <Link className="w-4 h-4" />
                          </button>
                          <button
                            onClick={() => void deleteItems([obj.key])}
                            disabled={busy}
                            title="Delete"
                            className={`${iconButton} hover:text-red-600 hover:bg-red-50`}
                          >
                            <Trash2 className="w-4 h-4" />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>

            {result?.isTruncated && (
              <div className="flex items-center justify-center gap-3 px-4 py-3 border-t border-gray-100">
                <button
                  onClick={() => void loadMore()}
                  disabled={loadingMore}
                  className="flex items-center gap-2 text-sm text-blue-600 hover:text-blue-700 font-medium disabled:opacity-50"
                >
                  {loadingMore && <Spinner size="sm" />}
                  Load more
                </button>
                {sort.field !== 'name' && (
                  <span className="text-xs text-gray-400">Sorting applies to the items loaded so far</span>
                )}
              </div>
            )}
          </div>
        )}
      </div>

      <UploadPanel
        tasks={uploadTasks}
        onCancel={cancelUpload}
        onCancelAll={cancelAllUploads}
        onDismiss={() => setUploadTasks([])}
      />

      {details && (
        <ObjectDetailsDrawer
          bucket={bucket}
          object={details}
          onClose={() => setDetails(null)}
          onDownload={(key) => void handleDownload(key)}
          onShare={(key) => setShare({ mode: 'download', key })}
          onDelete={(key) => void deleteItems([key])}
        />
      )}

      {share && (
        <ShareLinkModal
          bucket={bucket}
          initialKey={share.key}
          mode={share.mode}
          onClose={() => setShare(null)}
        />
      )}
    </div>
  );
}
