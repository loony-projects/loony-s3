import { useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { FlaskConical } from 'lucide-react';
import { ApiError } from '@/api/client';
import { consoleApi } from '@/api/console';
import { PageError, PageState } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatRupees } from '@/lib/format';
import { useAdminData } from './admin/shared';

/** GST is included in the price: this is the part of it that's tax, at 18%. */
function gstIncluded(paise: number): number {
  return Math.round(paise - paise / 1.18);
}

const field = 'w-full px-3 py-2 text-sm border border-gray-200 rounded-lg bg-gray-50 text-gray-500';

/**
 * A stand-in for a real payment page. It shows what one would, but its fields are fixed
 * placeholders and never sent anywhere: only the button pressed reaches the server.
 */
export function CheckoutPage() {
  const { checkoutId = '' } = useParams();
  const { data, error, loading } = useAdminData(() => consoleApi.checkout(checkoutId), [checkoutId]);
  const [method, setMethod] = useState<'card' | 'upi'>('card');
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const navigate = useNavigate();

  async function finish(outcome: 'succeed' | 'fail') {
    setBusy(true);
    try {
      const result = await consoleApi.confirmCheckout(checkoutId, outcome);
      if (result.checkout.status === 'failed') {
        toast.error('Payment failed. No credit was added.');
        navigate('/billing');
        return;
      }
      toast.success(`Payment received: ${formatRupees(result.checkout.amount_paise)} added`);
      if (result.subscription_error) toast.error(result.subscription_error);
      navigate(result.checkout.project_id ? `/projects/${result.checkout.project_id}/plan` : '/billing');
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'That didn\'t work');
      setBusy(false);
    }
  }

  return (
    <div className="max-w-md mx-auto px-4 py-10">
      <PageState error={error} loading={loading} hasData={data !== null}>
        {data && data.status !== 'pending' && (
          <div className="space-y-4">
            <PageError title="This payment is already finished">
              It {data.status === 'succeeded' ? 'went through' : 'failed'}. Start a new one if you need to.
            </PageError>
            <Link to="/billing" className="block text-center text-sm font-medium text-blue-600">Back to billing</Link>
          </div>
        )}
        {data && data.status === 'pending' && (
          <div className="bg-white border border-gray-200 rounded-2xl overflow-hidden">
            <div role="note" className="flex items-center gap-2 px-5 py-3 bg-amber-50 border-b border-amber-200 text-sm font-medium text-amber-900">
              <FlaskConical className="w-4 h-4 shrink-0" /> Test payment — no money is charged
            </div>
            <div className="p-5 space-y-5">
              <div>
                <p className="text-sm text-gray-500">{data.project_id ? 'Loony S3 subscription' : 'Loony S3 credit'}</p>
                <p className="text-3xl font-semibold text-gray-900 tabular-nums">{formatRupees(data.amount_paise)}</p>
                <p className="mt-1 text-xs text-gray-500">Includes {formatRupees(gstIncluded(data.amount_paise))} GST at 18%.</p>
              </div>

              <div role="tablist" aria-label="Payment method" className="flex gap-1 p-1 bg-gray-100 rounded-lg">
                {(['card', 'upi'] as const).map((option) => (
                  <button
                    key={option}
                    role="tab"
                    aria-selected={method === option}
                    onClick={() => setMethod(option)}
                    className={`flex-1 px-3 py-1.5 text-sm font-medium rounded-md transition-colors ${method === option ? 'bg-white text-gray-900 shadow-sm' : 'text-gray-500'}`}
                  >
                    {option === 'card' ? 'Card' : 'UPI'}
                  </button>
                ))}
              </div>

              {method === 'card' ? (
                <div className="space-y-3">
                  <div>
                    <label htmlFor="test-card" className="block text-sm text-gray-700 mb-1">Card number</label>
                    <input id="test-card" readOnly value="4111 1111 1111 1111" className={field} />
                  </div>
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label htmlFor="test-expiry" className="block text-sm text-gray-700 mb-1">Expiry</label>
                      <input id="test-expiry" readOnly value="12 / 34" className={field} />
                    </div>
                    <div>
                      <label htmlFor="test-cvc" className="block text-sm text-gray-700 mb-1">CVC</label>
                      <input id="test-cvc" readOnly value="123" className={field} />
                    </div>
                  </div>
                </div>
              ) : (
                <div>
                  <label htmlFor="test-upi" className="block text-sm text-gray-700 mb-1">UPI ID</label>
                  <input id="test-upi" readOnly value="test@upi" className={field} />
                </div>
              )}
              <p className="text-xs text-gray-500">These are fixed test details. Real card or UPI details are never asked for or stored.</p>

              <div className="grid grid-cols-2 gap-2">
                <button onClick={() => void finish('fail')} disabled={busy} className="px-4 py-2 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50">
                  Fail payment
                </button>
                <button onClick={() => void finish('succeed')} disabled={busy} className="flex items-center justify-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg">
                  {busy && <Spinner size="sm" />} Pay {formatRupees(data.amount_paise)}
                </button>
              </div>
            </div>
          </div>
        )}
      </PageState>
    </div>
  );
}
