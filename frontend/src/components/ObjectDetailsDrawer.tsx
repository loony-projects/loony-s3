import { useEffect, useState } from 'react';
import { X, Download, Link, Trash2 } from 'lucide-react';
import { headObject, presignObjectUrl, type ObjectHead, type ObjectSummary } from '@/api/objects';
import { ApiError } from '@/api/client';
import { formatBytes, formatDateTime, fileExtension } from '@/lib/format';
import { Spinner } from './Spinner';

type PreviewKind = 'image' | 'video' | 'audio' | 'pdf' | 'text';

const MAX_TEXT_PREVIEW_BYTES = 1024 * 1024;
const TEXT_TYPES = ['application/json', 'application/xml', 'application/javascript', 'application/x-yaml', 'application/yaml'];
const EXT_KINDS: Record<string, PreviewKind> = {
  png: 'image', jpg: 'image', jpeg: 'image', gif: 'image', webp: 'image', svg: 'image', bmp: 'image', avif: 'image',
  mp4: 'video', webm: 'video', mov: 'video',
  mp3: 'audio', wav: 'audio', ogg: 'audio', flac: 'audio', m4a: 'audio',
  pdf: 'pdf',
  txt: 'text', md: 'text', csv: 'text', json: 'text', log: 'text', yaml: 'text', yml: 'text', toml: 'text',
  xml: 'text', html: 'text', css: 'text', js: 'text', ts: 'text', rs: 'text', py: 'text', sh: 'text',
};

// The stored content type wins when it's specific; files uploaded without one arrive as
// application/octet-stream, so fall back to the extension.
function previewKind(contentType: string, key: string): PreviewKind | null {
  const ct = contentType.split(';')[0].trim().toLowerCase();
  if (ct.startsWith('image/')) return 'image';
  if (ct.startsWith('video/')) return 'video';
  if (ct.startsWith('audio/')) return 'audio';
  if (ct === 'application/pdf') return 'pdf';
  if (ct.startsWith('text/') || TEXT_TYPES.includes(ct)) return 'text';
  return EXT_KINDS[fileExtension(key)] ?? null;
}

function Preview({ kind, url, size }: { kind: PreviewKind | null; url: string; size: number }) {
  const [text, setText] = useState<string | null>(null);
  const [textError, setTextError] = useState(false);

  useEffect(() => {
    if (kind !== 'text' || size > MAX_TEXT_PREVIEW_BYTES) return;
    let cancelled = false;
    fetch(url)
      .then((r) => (r.ok ? r.text() : Promise.reject(new Error(String(r.status)))))
      .then((t) => { if (!cancelled) setText(t); })
      .catch(() => { if (!cancelled) setTextError(true); });
    return () => { cancelled = true; };
  }, [kind, url, size]);

  const box = 'rounded-lg border border-gray-200 bg-gray-50';
  switch (kind) {
    case 'image':
      return <img src={url} alt="" className={`${box} block mx-auto max-w-full max-h-96 object-contain`} />;
    case 'video':
      return <video src={url} controls className={`${box} w-full max-h-96`} />;
    case 'audio':
      return <audio src={url} controls className="w-full" />;
    case 'pdf':
      return <iframe src={url} title="PDF preview" className={`${box} w-full h-96`} />;
    case 'text':
      if (size > MAX_TEXT_PREVIEW_BYTES) return <p className="text-sm text-gray-500">Too large to preview (over 1 MB).</p>;
      if (textError) return <p className="text-sm text-red-600">Could not load the preview.</p>;
      if (text === null) return <div className="flex justify-center py-6"><Spinner /></div>;
      return <pre className={`${box} p-3 text-xs max-h-96 overflow-auto whitespace-pre-wrap break-words`}>{text}</pre>;
    default:
      return <p className="text-sm text-gray-500">No preview available for this file type.</p>;
  }
}

export function ObjectDetailsDrawer({ bucket, object, onClose, onDownload, onShare, onDelete }: {
  bucket: string;
  object: ObjectSummary;
  onClose: () => void;
  onDownload: (key: string) => void;
  onShare: (key: string) => void;
  onDelete: (key: string) => void;
}) {
  const [head, setHead] = useState<ObjectHead | null>(null);
  const [previewUrl, setPreviewUrl] = useState('');
  const [error, setError] = useState('');

  useEffect(() => {
    let cancelled = false;
    setHead(null);
    setPreviewUrl('');
    setError('');
    Promise.all([headObject(bucket, object.key), presignObjectUrl(bucket, object.key, 'GET', 900)])
      .then(([h, url]) => { if (!cancelled) { setHead(h); setPreviewUrl(url); } })
      .catch((err) => { if (!cancelled) setError(err instanceof ApiError ? err.message : 'Failed to load details'); });
    return () => { cancelled = true; };
  }, [bucket, object.key]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onClose]);

  const etag = head?.etag ?? object.etag;
  const multipartParts = /-(\d+)$/.exec(etag)?.[1];
  const kind = head ? previewKind(head.contentType, object.key) : null;
  const size = head?.contentLength ?? object.size;

  const rows: [string, string][] = [
    ['Size', `${formatBytes(size)} (${size.toLocaleString()} bytes)`],
    ['Type', head?.contentType || '—'],
    ['Last modified', formatDateTime(head?.lastModified || object.lastModified)],
    ['ETag', multipartParts ? `${etag} (uploaded in ${multipartParts} parts)` : etag],
  ];

  return (
    <div className="fixed inset-0 z-50 flex justify-end">
      <div className="absolute inset-0 bg-black/30" onClick={onClose} />
      <aside className="relative w-full max-w-lg h-full bg-white shadow-xl flex flex-col">
        <div className="flex items-start justify-between gap-3 px-5 py-4 border-b border-gray-100">
          <h2 className="text-base font-semibold text-gray-900 break-all">{object.key.split('/').pop()}</h2>
          <button onClick={onClose} title="Close" className="text-gray-400 hover:text-gray-600 shrink-0">
            <X className="w-5 h-5" />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto p-5 space-y-5">
          <div className="flex gap-2">
            <button onClick={() => onDownload(object.key)} className="flex items-center gap-1.5 px-3 py-1.5 text-sm border border-gray-200 rounded-lg hover:bg-gray-50">
              <Download className="w-4 h-4" /> Download
            </button>
            <button onClick={() => onShare(object.key)} className="flex items-center gap-1.5 px-3 py-1.5 text-sm border border-gray-200 rounded-lg hover:bg-gray-50">
              <Link className="w-4 h-4" /> Share link
            </button>
            <button onClick={() => onDelete(object.key)} className="flex items-center gap-1.5 px-3 py-1.5 text-sm text-red-600 border border-red-200 rounded-lg hover:bg-red-50">
              <Trash2 className="w-4 h-4" /> Delete
            </button>
          </div>

          <dl className="text-sm divide-y divide-gray-100 border border-gray-100 rounded-lg">
            <div className="px-3 py-2">
              <dt className="text-xs text-gray-500">Key</dt>
              <dd className="text-gray-800 break-all">{bucket}/{object.key}</dd>
            </div>
            {rows.map(([label, value]) => (
              <div key={label} className="px-3 py-2">
                <dt className="text-xs text-gray-500">{label}</dt>
                <dd className={`text-gray-800 break-all ${label === 'ETag' ? 'font-mono text-xs' : ''}`}>{value}</dd>
              </div>
            ))}
          </dl>

          <div>
            <h3 className="text-xs font-semibold text-gray-500 uppercase tracking-wide mb-2">Preview</h3>
            {error ? (
              <p className="text-sm text-red-600">{error}</p>
            ) : !head || !previewUrl ? (
              <div className="flex justify-center py-6"><Spinner /></div>
            ) : (
              <>
                <Preview key={previewUrl} kind={kind} url={previewUrl} size={size} />
                {(kind === 'video' || kind === 'audio') && (
                  <p className="mt-2 text-xs text-gray-400">
                    Seeking may not work yet: the server doesn't support byte-range requests.
                  </p>
                )}
              </>
            )}
          </div>
        </div>
      </aside>
    </div>
  );
}
