import { useState } from 'react';
import { Copy } from 'lucide-react';
import { presignObjectUrl } from '@/api/objects';
import { ApiError } from '@/api/client';
import { useToast } from '@/context/ToastContext';
import { Modal } from './Modal';
import { Spinner } from './Spinner';

export type ShareMode = 'download' | 'upload';

const EXPIRY_OPTIONS = [
  { label: '15 minutes', secs: 15 * 60 },
  { label: '1 hour', secs: 3600 },
  { label: '1 day', secs: 24 * 3600 },
  { label: '7 days (maximum)', secs: 7 * 24 * 3600 },
];

export function ShareLinkModal({ bucket, initialKey, mode, onClose }: {
  bucket: string;
  initialKey: string;
  mode: ShareMode;
  onClose: () => void;
}) {
  const toast = useToast();
  const [key, setKey] = useState(initialKey);
  const [expirySecs, setExpirySecs] = useState(3600);
  const [url, setUrl] = useState('');
  const [generating, setGenerating] = useState(false);

  const keyIsValid = key.trim() !== '' && !key.endsWith('/');

  async function generate() {
    setGenerating(true);
    try {
      setUrl(await presignObjectUrl(bucket, key.trim(), mode === 'upload' ? 'PUT' : 'GET', expirySecs));
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Failed to generate link');
    } finally {
      setGenerating(false);
    }
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(url);
      toast.success('Link copied');
    } catch {
      toast.error('Could not copy to clipboard -- select the link and copy it manually');
    }
  }

  const expiryLabel = EXPIRY_OPTIONS.find((o) => o.secs === expirySecs)?.label ?? '';

  return (
    <Modal
      title={mode === 'upload' ? 'Create upload link' : 'Share download link'}
      onClose={onClose}
      size="lg"
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800">
            Close
          </button>
          <button
            onClick={() => void generate()}
            disabled={generating || !keyIsValid}
            className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg"
          >
            {generating && <Spinner size="sm" />}
            {url ? 'Regenerate' : 'Generate link'}
          </button>
        </>
      }
    >
      <div className="space-y-4">
        <p className="text-sm text-gray-500">
          {mode === 'upload'
            ? 'Anyone with this link can upload one file to the key below (overwriting it if it exists) until the link expires. No sign-in needed.'
            : 'Anyone with this link can download this object until the link expires. No sign-in needed.'}
        </p>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-1.5">Object key</label>
          {mode === 'upload' ? (
            <input
              value={key}
              onChange={(e) => { setKey(e.target.value); setUrl(''); }}
              placeholder="folder/file-name.ext"
              className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
          ) : (
            <p className="text-sm text-gray-800 break-all">{key}</p>
          )}
          {mode === 'upload' && !keyIsValid && key !== '' && (
            <p className="mt-1 text-xs text-red-600">Enter a file name, not a folder.</p>
          )}
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700 mb-1.5">Expires after</label>
          <select
            value={expirySecs}
            onChange={(e) => { setExpirySecs(Number(e.target.value)); setUrl(''); }}
            className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500"
          >
            {EXPIRY_OPTIONS.map((o) => <option key={o.secs} value={o.secs}>{o.label}</option>)}
          </select>
        </div>

        {url && (
          <div className="space-y-2">
            <div className="flex gap-2">
              <input
                readOnly
                value={url}
                onFocus={(e) => e.target.select()}
                className="flex-1 min-w-0 px-3 py-2 text-xs font-mono border border-gray-300 rounded-lg bg-gray-50"
              />
              <button
                onClick={() => void copy()}
                className="flex items-center gap-1.5 px-3 py-2 text-sm font-medium text-blue-600 border border-blue-200 rounded-lg hover:bg-blue-50"
              >
                <Copy className="w-4 h-4" /> Copy
              </button>
            </div>
            <p className="text-xs text-gray-400">Valid for {expiryLabel} from now.</p>
            {mode === 'upload' && (
              <div>
                <p className="text-xs text-gray-500 mb-1">Upload with curl:</p>
                <pre className="text-xs bg-gray-50 border border-gray-200 rounded-lg p-2 overflow-x-auto">
                  {`curl -X PUT --upload-file ./your-file "${url}"`}
                </pre>
              </div>
            )}
          </div>
        )}
      </div>
    </Modal>
  );
}
