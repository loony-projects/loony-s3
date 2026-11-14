import { consoleApi } from '@/api/console';
import { PageState } from '@/components/PageError';
import { perPeriod, SubscriptionActions, SubscriptionBadge, UsageBar } from '@/components/SubscriptionPanel';
import { formatBytes, formatDate, formatRupees } from '@/lib/format';
import { useAdminData } from '../admin/shared';
import { useProject } from './ProjectLayout';

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col sm:flex-row sm:gap-4 py-2">
      <dt className="sm:w-40 shrink-0 text-sm text-gray-500">{label}</dt>
      <dd className="text-sm text-gray-900 min-w-0 break-words">{children}</dd>
    </div>
  );
}

export function PlanPage() {
  const { project, subscription, reload } = useProject();
  const billing = useAdminData(consoleApi.billing);
  const isOwner = project.role === 'owner';
  const plan = subscription.plan ?? billing.data?.plans[0] ?? null;

  const refresh = async () => {
    await reload();
    await billing.reload();
  };

  return (
    <div className="max-w-2xl space-y-5">
      <h2 className="text-lg font-semibold text-gray-900">Plan</h2>

      <section className="bg-white border border-gray-200 rounded-xl p-5">
        <div className="flex items-center justify-between gap-2 mb-3">
          <h3 className="text-sm font-medium text-gray-900">Storage</h3>
          <SubscriptionBadge state={subscription.state} />
        </div>
        <UsageBar subscription={subscription} />
      </section>

      <section className="bg-white border border-gray-200 rounded-xl p-5">
        <dl className="divide-y divide-gray-100">
          {plan && (
            <Row label="Plan">
              {plan.name}: {formatBytes(plan.storage_bytes)} for {formatRupees(plan.price_paise)}
              {perPeriod(plan)}, including GST
            </Row>
          )}
          {subscription.state === 'active' && subscription.period_end && (
            <Row label={subscription.cancel_at_period_end ? 'Ends' : 'Renews'}>{formatDate(subscription.period_end)}</Row>
          )}
          {subscription.read_only_at && <Row label="Read-only from">{formatDate(subscription.read_only_at)}</Row>}
          {subscription.suspends_at && <Row label="Suspended from">{formatDate(subscription.suspends_at)}</Row>}
          {subscription.state !== 'unpaid' && (
            <Row label="Paid by">{subscription.payer_is_me ? 'You' : subscription.payer_email ?? 'Another Owner'}</Row>
          )}
        </dl>
        {subscription.state === 'active' && !subscription.cancel_at_period_end && (
          <p className="mt-3 text-xs text-gray-500">Renewals are paid automatically from {subscription.payer_is_me ? 'your' : 'the payer\'s'} credit.</p>
        )}
      </section>

      {isOwner ? (
        <PageState error={billing.error} loading={billing.loading} hasData={billing.data !== null}>
          {billing.data && project.enabled && (
            <section className="bg-white border border-gray-200 rounded-xl p-5">
              <SubscriptionActions projectId={project.id} subscription={subscription} billing={billing.data} onChange={refresh} />
            </section>
          )}
        </PageState>
      ) : (
        subscription.state !== 'active' && <p className="text-sm text-gray-600">Only a project Owner can pay for it.</p>
      )}
    </div>
  );
}
