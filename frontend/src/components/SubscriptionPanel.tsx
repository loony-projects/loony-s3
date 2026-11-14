import { ReactNode, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { AlertTriangle, CreditCard } from 'lucide-react';
import { ApiError } from '@/api/client';
import { BillingOverview, consoleApi, Subscription, SubscriptionState } from '@/api/console';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatBytes, formatDate, formatRupees, formatUsage } from '@/lib/format';
import { Badge } from '@/pages/admin/shared';

const stateLabel: Record<SubscriptionState, string> = {
  unpaid: 'Unpaid',
  active: 'Active',
  grace: 'Payment due',
  read_only: 'Read-only',
  suspended: 'Suspended',
};
const stateTone = { unpaid: 'gray', active: 'green', grace: 'amber', read_only: 'amber', suspended: 'red' } as const;

export function SubscriptionBadge({ state }: { state: SubscriptionState }) {
  return <Badge tone={stateTone[state]}>{stateLabel[state]}</Badge>;
}

export function perPeriod(subscriptionPlan: { period: { months: number } | { seconds: number } }): string {
  const period = subscriptionPlan.period;
  if ('months' in period) return period.months === 1 ? '/month' : ` every ${period.months} months`;
  return ` every ${period.seconds} seconds`;
}

/** What a project's state means for the people in it, shown at the top of its pages. */
export function SubscriptionBanner({ subscription, projectId, isOwner }: { subscription: Subscription; projectId: string; isOwner: boolean }) {
  const plan = `/projects/${projectId}/plan`;
  const action = (label: string) =>
    isOwner ? (
      <Link to={plan} className="font-medium underline underline-offset-2 whitespace-nowrap">{label}</Link>
    ) : (
      <span>Ask a project Owner to renew it.</span>
    );
  let tone: 'blue' | 'amber' | 'red';
  let text: ReactNode;
  switch (subscription.state) {
    case 'unpaid':
      tone = 'blue';
      text = <>This project isn't paid for yet, so it can't store anything. {action('Subscribe')}</>;
      break;
    case 'grace':
      tone = 'amber';
      text = (
        <>
          The renewal couldn't be paid. Everything still works until {subscription.read_only_at ? formatDate(subscription.read_only_at) : 'soon'}, then the
          project becomes read-only. {action('Renew now')}
        </>
      );
      break;
    case 'read_only':
      tone = 'amber';
      text = (
        <>
          This project is read-only: files can be viewed and downloaded, but not changed.
          {subscription.suspends_at && ` It will be suspended on ${formatDate(subscription.suspends_at)}.`} {action('Renew now')}
        </>
      );
      break;
    case 'suspended':
      tone = 'red';
      text = <>This project is suspended. Nothing has been deleted. {action('Renew to use it again')}</>;
      break;
    default:
      if (!subscription.cancel_at_period_end || !subscription.period_end) return null;
      tone = 'blue';
      text = <>This subscription is cancelled. The project becomes read-only on {formatDate(subscription.period_end)}.</>;
  }
  const tones = {
    blue: 'bg-blue-50 border-blue-200 text-blue-900',
    amber: 'bg-amber-50 border-amber-200 text-amber-900',
    red: 'bg-red-50 border-red-200 text-red-900',
  };
  return (
    <div role="status" className={`flex gap-2 border rounded-xl px-4 py-3 text-sm ${tones[tone]}`}>
      <AlertTriangle className="w-4 h-4 shrink-0 mt-0.5" />
      <p className="min-w-0">{text}</p>
    </div>
  );
}

export function UsageBar({ subscription }: { subscription: Subscription }) {
  const { bytes, pending_bytes: pending, limit_bytes: limit } = subscription.usage;
  const used = bytes + pending;
  const percent = limit ? Math.min(100, (used / limit) * 100) : 0;
  return (
    <div>
      <div className="flex items-baseline justify-between gap-2 text-sm">
        <span className="text-gray-900 font-medium">{formatUsage(used, limit)}</span>
        <span className="text-gray-500">{subscription.usage.objects.toLocaleString()} files</span>
      </div>
      {limit !== null && (
        <div className="mt-2 h-2 bg-gray-100 rounded-full overflow-hidden" role="progressbar" aria-valuenow={Math.round(percent)} aria-valuemin={0} aria-valuemax={100} aria-label="Storage used">
          <div className={`h-full rounded-full ${percent >= 90 ? 'bg-amber-500' : 'bg-blue-600'}`} style={{ width: `${percent}%` }} />
        </div>
      )}
      {pending > 0 && <p className="mt-1 text-xs text-gray-500">Includes {formatBytes(pending)} of uploads in progress.</p>}
    </div>
  );
}

function needsPayment(subscription: Subscription) {
  return subscription.state !== 'active';
}

/** Subscribe, renew, cancel and resume, for a project's Owner. */
export function SubscriptionActions({
  projectId,
  subscription,
  billing,
  onChange,
}: {
  projectId: string;
  subscription: Subscription;
  billing: BillingOverview;
  onChange: () => Promise<void> | void;
}) {
  const toast = useToast();
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  const [chosen, setChosen] = useState(billing.plans[0]?.id ?? '');
  const choosing = subscription.state === 'unpaid' && billing.plans.length > 1;
  const plan = subscription.plan ?? billing.plans.find((p) => p.id === chosen) ?? billing.plans[0];
  const price = plan?.price_paise ?? 0;
  const shortfall = Math.max(0, price - billing.balance_paise);

  async function run(action: () => Promise<unknown>, success: string) {
    setBusy(true);
    try {
      await action();
      toast.success(success);
      await onChange();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'That didn\'t work');
    } finally {
      setBusy(false);
    }
  }

  async function payShortfall() {
    setBusy(true);
    try {
      const checkout = await consoleApi.startCheckout(shortfall, projectId, plan?.id);
      navigate(`/billing/checkout/${checkout.id}`);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Couldn\'t start the payment');
      setBusy(false);
    }
  }

  if (!plan) return null;
  const primary = 'flex items-center justify-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors';
  const secondary = 'px-4 py-2 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50 transition-colors';

  const picker = choosing && (
    <div className="mb-3">
      <label htmlFor={`plan-${projectId}`} className="block text-sm font-medium text-gray-700 mb-1.5">Plan</label>
      <select id={`plan-${projectId}`} value={plan.id} onChange={(e) => setChosen(e.target.value)} className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500">
        {billing.plans.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}: {formatBytes(p.storage_bytes)} for {formatRupees(p.price_paise)}{perPeriod(p)}
          </option>
        ))}
      </select>
    </div>
  );

  if (needsPayment(subscription)) {
    const label = subscription.state === 'unpaid' ? `Subscribe for ${formatRupees(price)}${perPeriod(plan)}` : `Renew now for ${formatRupees(price)}`;
    const pay = subscription.state === 'unpaid' ? () => consoleApi.subscribe(projectId, plan.id) : () => consoleApi.renew(projectId);
    if (shortfall === 0) {
      return (
        <div>
          {picker}
          <button className={primary} disabled={busy} onClick={() => void run(pay, subscription.state === 'unpaid' ? 'Subscribed' : 'Renewed')}>
            {busy && <Spinner size="sm" />} {label}
          </button>
        </div>
      );
    }
    return (
      <div className="space-y-3">
        {picker}
        <p className="text-sm text-gray-700">
          {label.replace(/ now/, '')} needs {formatRupees(price)}. Your credit is {formatRupees(billing.balance_paise)}.
        </p>
        <div className="flex flex-wrap gap-2">
          {billing.checkout && (
            <button className={primary} disabled={busy} onClick={() => void payShortfall()}>
              <CreditCard className="w-4 h-4" /> Pay {formatRupees(shortfall)}
            </button>
          )}
          <Link to="/billing" className={secondary}>Redeem a gift code</Link>
        </div>
      </div>
    );
  }
  if (subscription.cancel_at_period_end) {
    return (
      <button className={secondary} disabled={busy} onClick={() => void run(() => consoleApi.resumeSubscription(projectId), 'Subscription resumed')}>
        Keep subscription
      </button>
    );
  }
  return (
    <button
      className={secondary}
      disabled={busy}
      onClick={() => {
        const end = subscription.period_end ? formatDate(subscription.period_end) : 'the end of this period';
        if (confirm(`Cancel this subscription? The project works until ${end}, then becomes read-only. Nothing is deleted.`)) {
          void run(() => consoleApi.cancelSubscription(projectId), 'Subscription cancelled');
        }
      }}
    >
      Cancel subscription
    </button>
  );
}
