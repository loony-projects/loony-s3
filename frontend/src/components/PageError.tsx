import { ReactNode } from 'react';
import { AlertTriangle } from 'lucide-react';
import { ApiError } from '@/api/client';
import { Spinner } from '@/components/Spinner';

export function PageError({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div role="alert" className="flex gap-3 bg-amber-50 border border-amber-200 rounded-xl p-5">
      <AlertTriangle className="w-5 h-5 text-amber-600 shrink-0 mt-0.5" />
      <div>
        <h2 className="text-sm font-medium text-amber-900">{title}</h2>
        <p className="text-sm text-amber-800 mt-1">{children}</p>
      </div>
    </div>
  );
}

/** Shows a page's loading and failure states, so each page only describes its content. */
export function PageState({ error, loading, hasData, children }: { error: unknown; loading: boolean; hasData: boolean; children: ReactNode }) {
  if (error) {
    return <PageError title="That didn't load">{error instanceof ApiError ? error.message : 'The request failed.'}</PageError>;
  }
  if (loading && !hasData) {
    return <div className="flex justify-center py-20"><Spinner size="lg" /></div>;
  }
  return <>{children}</>;
}
