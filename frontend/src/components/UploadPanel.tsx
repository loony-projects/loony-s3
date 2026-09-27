import { X } from 'lucide-react';
import { formatBytes } from '@/lib/format';

export type UploadStatus = 'queued' | 'uploading' | 'done' | 'error' | 'cancelled';

export interface UploadTask {
  id: number;
  name: string;
  loaded: number;
  total: number;
  status: UploadStatus;
  error?: string;
}

const isActive = (t: UploadTask) => t.status === 'queued' || t.status === 'uploading';

function statusLabel(t: UploadTask): string {
  switch (t.status) {
    case 'queued': return 'Queued';
    case 'uploading': return t.total > 0 ? `${Math.round((t.loaded / t.total) * 100)}%` : '…';
    case 'done': return 'Done';
    case 'error': return 'Failed';
    case 'cancelled': return 'Cancelled';
  }
}

const barColor: Record<UploadStatus, string> = {
  queued: 'bg-gray-300',
  uploading: 'bg-blue-500',
  done: 'bg-green-500',
  error: 'bg-red-400',
  cancelled: 'bg-gray-400',
};

export function UploadPanel({ tasks, onCancel, onCancelAll, onDismiss }: {
  tasks: UploadTask[];
  onCancel: (id: number) => void;
  onCancelAll: () => void;
  onDismiss: () => void;
}) {
  if (tasks.length === 0) return null;

  const active = tasks.filter(isActive).length;
  const finished = tasks.filter((t) => t.status === 'done').length;
  const totalBytes = tasks.reduce((a, t) => a + t.total, 0);
  const loadedBytes = tasks.reduce((a, t) => a + (t.status === 'done' ? t.total : t.loaded), 0);

  return (
    <div className="fixed bottom-4 left-4 w-80 bg-white border border-gray-200 rounded-xl shadow-lg z-40">
      <div className="flex items-center justify-between px-4 py-3 border-b border-gray-100">
        <div>
          <p className="text-sm font-semibold text-gray-800">
            {active > 0 ? `Uploading ${finished}/${tasks.length}` : `Uploads ${finished}/${tasks.length} done`}
          </p>
          <p className="text-xs text-gray-400">{formatBytes(loadedBytes)} of {formatBytes(totalBytes)}</p>
        </div>
        {active > 0 ? (
          <button onClick={onCancelAll} className="text-xs font-medium text-red-600 hover:text-red-700">
            Cancel all
          </button>
        ) : (
          <button onClick={onDismiss} title="Close" className="text-gray-400 hover:text-gray-600">
            <X className="w-4 h-4" />
          </button>
        )}
      </div>
      <div className="max-h-64 overflow-y-auto px-4 py-2 space-y-2">
        {tasks.map((t) => (
          <div key={t.id} title={t.error}>
            <div className="flex items-center justify-between gap-2 text-xs text-gray-600 mb-1">
              <span className="truncate">{t.name}</span>
              <span className="flex items-center gap-1 shrink-0">
                <span className={t.status === 'error' ? 'text-red-600' : ''}>{statusLabel(t)}</span>
                {isActive(t) && (
                  <button onClick={() => onCancel(t.id)} title="Cancel" className="text-gray-400 hover:text-red-600">
                    <X className="w-3.5 h-3.5" />
                  </button>
                )}
              </span>
            </div>
            <div className="h-1.5 bg-gray-100 rounded-full overflow-hidden">
              <div
                className={`h-full rounded-full transition-all ${barColor[t.status]}`}
                style={{
                  width: `${t.status === 'uploading' && t.total > 0 ? (t.loaded / t.total) * 100 : t.status === 'queued' ? 0 : 100}%`,
                }}
              />
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
