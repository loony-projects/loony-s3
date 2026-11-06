import { useState } from 'react';
import { Check, Copy } from 'lucide-react';
import { useToast } from '@/context/ToastContext';

export function CopyField({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  const toast = useToast();

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast.error('Could not copy. Select the text and copy it manually.');
    }
  }

  return (
    <div>
      <p className="text-sm font-medium text-gray-700 mb-1.5">{label}</p>
      <div className="flex gap-2">
        <code className="flex-1 px-3 py-2 text-xs bg-gray-50 border border-gray-200 rounded-lg break-all select-all">{value}</code>
        <button onClick={() => void copy()} aria-label={`Copy ${label.toLowerCase()}`} className="px-3 text-gray-500 hover:text-gray-800 border border-gray-200 rounded-lg transition-colors">
          {copied ? <Check className="w-4 h-4 text-green-600" /> : <Copy className="w-4 h-4" />}
        </button>
      </div>
    </div>
  );
}
