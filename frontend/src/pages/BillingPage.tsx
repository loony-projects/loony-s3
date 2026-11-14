import { FormEvent, useEffect, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { Gift, Wallet } from 'lucide-react';
import { ApiError } from '@/api/client';
import { consoleApi, Project, Subscription } from '@/api/console';
import { PageState } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { SubscriptionActions, SubscriptionBadge } from '@/components/SubscriptionPanel';
import { useToast } from '@/context/ToastContext';
import { formatDate, formatRupees, formatUsage } from '@/lib/format';
import { Table, useAdminData } from './admin/shared';

function RedeemForm({ onRedeemed }: { onRedeemed: () => void }) {
  const [code, setCode] = useState('');
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      const result = await consoleApi.redeem(code);
      toast.success(`Gift code redeemed: ${formatRupees(result.amount_paise)} added`);
      setCode('');
      onRedeemed();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'That gift code didn\'t work');
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="flex flex-col sm:flex-row gap-2">
      <label htmlFor="gift-code" className="sr-only">Gift code</label>
      <input
        id="gift-code"
        value={code}
        onChange={(e) => setCode(e.target.value)}
        placeholder="Gift code"
        autoComplete="off"
        spellCheck={false}
        className="flex-1 min-w-0 px-3 py-2 text-sm font-mono uppercase border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
      />
      <button type="submit" disabled={busy || !code.trim()} className="flex items-center justify-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
        {busy && <Spinner size="sm" />} Redeem
      </button>
    </form>
  );
}

function AddCredit() {
  const [amount, setAmount] = useState('1000');
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const navigate = useNavigate();
  const rupees = Number(amount);
  const valid = Number.isInteger(rupees) && rupees >= 1 && rupees <= 100000;

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      const checkout = await consoleApi.startCheckout(rupees * 100);
      navigate(`/billing/checkout/${checkout.id}`);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : 'Couldn\'t start the payment');
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="flex flex-col sm:flex-row gap-2">
      <label htmlFor="credit-amount" className="sr-only">Amount in rupees</label>
      <div className="flex flex-1 min-w-0 items-center border border-gray-300 rounded-lg focus-within:ring-2 focus-within:ring-blue-500">
        <span className="pl-3 text-sm text-gray-500">₹</span>
        <input id="credit-amount" inputMode="numeric" value={amount} onChange={(e) => setAmount(e.target.value.replace(/[^0-9]/g, ''))} className="flex-1 min-w-0 px-2 py-2 text-sm rounded-lg focus:outline-none" />
      </div>
      <button type="submit" disabled={busy || !valid} className="px-4 py-2 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50 transition-colors">
        Add credit
      </button>
    </form>
  );
}

function OwnedProjects({ onChange }: { onChange: () => Promise<void> }) {
  const billing = useAdminData(consoleApi.billing);
  const [rows, setRows] = useState<{ project: Project; subscription: Subscription }[] | null>(null);
  const [error, setError] = useState<unknown>(null);

  const load = async () => {
    try {
      const list = await consoleApi.projects();
      const owned = [...list.created, ...list.joined].filter((p) => p.role === 'owner');
      setRows(await Promise.all(owned.map(async (project) => ({ project, subscription: await consoleApi.subscription(project.id) }))));
      setError(null);
    } catch (err) {
      setError(err);
    }
  };
  useEffect(() => {
    void load();
  }, []);

  const refresh = async () => {
    await Promise.all([load(), billing.reload(), onChange()]);
  };

  return (
    <PageState error={error ?? billing.error} loading={rows === null || billing.loading} hasData={rows !== null && billing.data !== null}>
      {rows && rows.length === 0 && <p className="text-sm text-gray-500">You don't own any projects yet.</p>}
      <div className="space-y-3">
        {rows?.map(({ project, subscription }) => (
          <div key={project.id} className="bg-white border border-gray-200 rounded-xl p-4">
            <div className="flex flex-wrap items-center justify-between gap-2 mb-3">
              <Link to={`/projects/${project.id}/plan`} className="text-sm font-medium text-gray-900 hover:text-blue-600 break-words min-w-0">{project.name}</Link>
              <SubscriptionBadge state={subscription.state} />
            </div>
            <p className="text-sm text-gray-600 mb-3">
              {formatUsage(subscription.usage.bytes + subscription.usage.pending_bytes, subscription.usage.limit_bytes)}
              {subscription.state === 'active' && subscription.period_end &&
                ` · ${subscription.cancel_at_period_end ? 'ends' : 'renews'} ${formatDate(subscription.period_end)}`}
            </p>
            {billing.data && project.enabled && (
              <SubscriptionActions projectId={project.id} subscription={subscription} billing={billing.data} onChange={refresh} />
            )}
          </div>
        ))}
      </div>
    </PageState>
  );
}

export function BillingPage() {
  const billing = useAdminData(consoleApi.billing);
  const ledger = useAdminData(consoleApi.ledger);
  const reload = async () => {
    await Promise.all([billing.reload(), ledger.reload()]);
  };

  return (
    <div className="max-w-4xl mx-auto px-4 sm:px-6 lg:px-8 py-8 space-y-8">
      <h1 className="text-xl font-semibold text-gray-900">Billing</h1>

      <PageState error={billing.error} loading={billing.loading} hasData={billing.data !== null}>
        {billing.data && (
          <section className="grid gap-4 sm:grid-cols-2">
            <div className="bg-white border border-gray-200 rounded-xl p-5">
              <div className="flex items-center gap-2 text-sm text-gray-500"><Wallet className="w-4 h-4" /> Credit</div>
              <p className="mt-2 text-2xl font-semibold text-gray-900 tabular-nums">{formatRupees(billing.data.balance_paise)}</p>
              <p className="mt-1 text-xs text-gray-500">Subscriptions are paid from this.</p>
              {billing.data.checkout && <div className="mt-4"><AddCredit /></div>}
            </div>
            <div className="bg-white border border-gray-200 rounded-xl p-5">
              <div className="flex items-center gap-2 text-sm text-gray-500 mb-3"><Gift className="w-4 h-4" /> Redeem a gift code</div>
              <RedeemForm onRedeemed={() => void reload()} />
            </div>
          </section>
        )}
      </PageState>

      <section>
        <h2 className="text-sm font-medium text-gray-500 mb-3">Your projects</h2>
        <OwnedProjects onChange={reload} />
      </section>

      <section>
        <h2 className="text-sm font-medium text-gray-500 mb-3">History</h2>
        <PageState error={ledger.error} loading={ledger.loading} hasData={ledger.data !== null}>
          {ledger.data && ledger.data.length === 0 && <p className="text-sm text-gray-500">Nothing yet.</p>}
          {ledger.data && ledger.data.length > 0 && (
            <Table headers={['Date', 'Description', 'Amount', 'Balance']}>
              {ledger.data.map((entry) => (
                <tr key={entry.id}>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(entry.at)}</td>
                  <td className="px-4 py-3 text-gray-900 min-w-[10rem] break-words">{entry.description}</td>
                  <td className={`px-4 py-3 tabular-nums whitespace-nowrap ${entry.amount_paise > 0 ? 'text-green-700' : 'text-gray-900'}`}>
                    {entry.amount_paise > 0 ? '+' : '−'}{formatRupees(Math.abs(entry.amount_paise))}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-gray-500 whitespace-nowrap">{formatRupees(entry.balance_after_paise)}</td>
                </tr>
              ))}
            </Table>
          )}
        </PageState>
      </section>
    </div>
  );
}
