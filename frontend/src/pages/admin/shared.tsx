import { ReactNode, useCallback, useEffect, useState } from 'react';
import { AlertTriangle, RefreshCw } from 'lucide-react';
import { ApiError } from '@/api/client';
import { ADMIN_ENDPOINT } from '@/api/admin';
import { useToast } from '@/context/ToastContext';
import { Spinner } from '@/components/Spinner';

export function errorMessage(err: unknown, fallback: string): string {
  return err instanceof ApiError ? err.message : fallback;
}

/** Loads on mount and on `reload`, keeping the current data on screen during a reload. */
export function useAdminData<T>(fetcher: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);

  // eslint-disable-next-line react-hooks/exhaustive-deps
  const stableFetcher = useCallback(fetcher, deps);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      setData(await stableFetcher());
      setError(null);
    } catch (err) {
      setError(err);
    } finally {
      setLoading(false);
    }
  }, [stableFetcher]);

  useEffect(() => {
    void reload();
  }, [reload]);

  return { data, error, loading, reload };
}

/** Runs an admin action, reporting the outcome by toast and refreshing what's on screen. */
export function useAdminAction(reload: () => Promise<void>) {
  const toast = useToast();
  const [busy, setBusy] = useState<string | null>(null);

  async function run(id: string, action: () => Promise<unknown>, success: string) {
    setBusy(id);
    try {
      await action();
      toast.success(success);
      await reload();
    } catch (err) {
      toast.error(errorMessage(err, 'That action failed'));
    } finally {
      setBusy(null);
    }
  }

  return { busy, run };
}

function explainError(error: unknown): { title: string; detail: string } {
  if (error instanceof ApiError && error.status === 0) {
    return {
      title: 'The admin API is out of reach',
      detail: `It only accepts connections from the machine running the server, at ${ADMIN_ENDPOINT}. Open this panel on that machine (or through an SSH tunnel), from a localhost address, and check the server is running.`,
    };
  }
  if (error instanceof ApiError && (error.status === 401 || error.status === 403)) {
    return {
      title: 'This key has no admin access',
      detail: 'Sign in with the root access key configured for the server.',
    };
  }
  return { title: 'Something went wrong', detail: errorMessage(error, 'The request failed.') };
}

export function AdminPageState({ error, loading, hasData, children }: { error: unknown; loading: boolean; hasData: boolean; children: ReactNode }) {
  if (error) {
    const { title, detail } = explainError(error);
    return (
      <div role="alert" className="flex gap-3 bg-amber-50 border border-amber-200 rounded-xl p-5">
        <AlertTriangle className="w-5 h-5 text-amber-600 shrink-0 mt-0.5" />
        <div>
          <h2 className="text-sm font-medium text-amber-900">{title}</h2>
          <p className="text-sm text-amber-800 mt-1">{detail}</p>
        </div>
      </div>
    );
  }
  if (loading && !hasData) {
    return <div className="flex justify-center py-20"><Spinner size="lg" /></div>;
  }
  return <>{children}</>;
}

export function PageHeader({ title, description, onRefresh, loading, children }: {
  title: string;
  description?: string;
  onRefresh?: () => void;
  loading?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-start sm:justify-between gap-4 mb-5">
      <div>
        <h2 className="text-lg font-semibold text-gray-900">{title}</h2>
        {description && <p className="text-sm text-gray-500 mt-0.5 max-w-2xl">{description}</p>}
      </div>
      <div className="flex items-center gap-2 shrink-0">
        {onRefresh && (
          <button
            onClick={onRefresh}
            disabled={loading}
            className="flex items-center gap-1.5 px-3 py-2 text-sm text-gray-600 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50 transition-colors"
          >
            <RefreshCw className={`w-4 h-4 ${loading ? 'animate-spin' : ''}`} />
            Refresh
          </button>
        )}
        {children}
      </div>
    </div>
  );
}

export function Table({ headers, children }: { headers: string[]; children: ReactNode }) {
  return (
    <div className="bg-white border border-gray-200 rounded-xl overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-gray-100 text-left text-xs font-medium text-gray-500 uppercase tracking-wide">
            {headers.map((h) => <th key={h} scope="col" className="px-4 py-3 whitespace-nowrap">{h}</th>)}
          </tr>
        </thead>
        <tbody className="divide-y divide-gray-100">{children}</tbody>
      </table>
    </div>
  );
}

type BadgeTone = 'green' | 'blue' | 'amber' | 'red' | 'gray';

const toneClasses: Record<BadgeTone, string> = {
  green: 'bg-green-50 text-green-700',
  blue: 'bg-blue-50 text-blue-700',
  amber: 'bg-amber-50 text-amber-700',
  red: 'bg-red-50 text-red-700',
  gray: 'bg-gray-100 text-gray-600',
};

export function Badge({ tone, children }: { tone: BadgeTone; children: ReactNode }) {
  return <span className={`inline-block px-2 py-0.5 text-xs font-medium rounded-full whitespace-nowrap ${toneClasses[tone]}`}>{children}</span>;
}

export const secondaryButton =
  'px-2.5 py-1.5 text-xs font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-40 disabled:cursor-not-allowed transition-colors';
export const dangerButton =
  'px-2.5 py-1.5 text-xs font-medium text-red-700 bg-white border border-red-200 rounded-lg hover:bg-red-50 disabled:opacity-40 disabled:cursor-not-allowed transition-colors';

/** Long ids stay fully readable on hover and copyable, without dominating a table. */
export function ShortId({ value }: { value: string }) {
  return <code className="text-xs text-gray-600" title={value}>{value.slice(0, 8)}…</code>;
}
