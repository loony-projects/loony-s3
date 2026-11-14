import { FormEvent, Fragment, useState } from 'react';
import { Gift, Plus } from 'lucide-react';
import { adminApi, GiftCode } from '@/api/admin';
import { CopyField } from '@/components/CopyField';
import { EmptyState } from '@/components/EmptyState';
import { Modal } from '@/components/Modal';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatDate, formatDateTime, formatRupees } from '@/lib/format';
import { AdminPageState, Badge, PageHeader, Table, dangerButton, errorMessage, secondaryButton, useAdminAction, useAdminData } from './shared';

function status(code: GiftCode): { label: string; tone: 'green' | 'gray' | 'amber' } {
  if (code.disabled) return { label: 'Disabled', tone: 'gray' };
  if (code.expires_at && new Date(code.expires_at) <= new Date()) return { label: 'Expired', tone: 'gray' };
  if (code.uses >= code.max_uses) return { label: 'Used up', tone: 'amber' };
  return { label: 'Active', tone: 'green' };
}

function CreateModal({ onClose, onCreated }: { onClose: () => void; onCreated: (code: string, amount: number) => void }) {
  const [amount, setAmount] = useState('20000');
  const [uses, setUses] = useState('1');
  const [expires, setExpires] = useState('');
  const [note, setNote] = useState('');
  const [saving, setSaving] = useState(false);
  const toast = useToast();
  const rupees = Number(amount);
  const maxUses = Number(uses);
  const valid = Number.isInteger(rupees) && rupees >= 1 && rupees <= 1000000 && Number.isInteger(maxUses) && maxUses >= 1;

  async function submit(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    try {
      const created = await adminApi.createGiftCode({
        amount_paise: rupees * 100,
        max_uses: maxUses,
        // The end of the chosen day, in this browser's time zone.
        expires_at: expires ? new Date(`${expires}T23:59:59`).toISOString() : null,
        note: note.trim() || null,
      });
      onCreated(created.code, created.amount_paise);
    } catch (err) {
      toast.error(errorMessage(err, 'Could not create the code'));
      setSaving(false);
    }
  }

  const control = 'w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500';
  return (
    <Modal
      title="Create gift code"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
          <button form="gift-code-form" type="submit" disabled={saving || !valid} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {saving && <Spinner size="sm" />} Create
          </button>
        </>
      }
    >
      <form id="gift-code-form" onSubmit={(e) => void submit(e)} className="space-y-4">
        <div>
          <label htmlFor="gift-amount" className="block text-sm font-medium text-gray-700 mb-1.5">Amount (₹)</label>
          <input id="gift-amount" inputMode="numeric" value={amount} onChange={(e) => setAmount(e.target.value.replace(/[^0-9]/g, ''))} className={control} />
        </div>
        <div>
          <label htmlFor="gift-uses" className="block text-sm font-medium text-gray-700 mb-1.5">How many people can use it</label>
          <input id="gift-uses" inputMode="numeric" value={uses} onChange={(e) => setUses(e.target.value.replace(/[^0-9]/g, ''))} className={control} />
          <p className="mt-1 text-xs text-gray-500">Each person can redeem it once.</p>
        </div>
        <div>
          <label htmlFor="gift-expires" className="block text-sm font-medium text-gray-700 mb-1.5">Expires (optional)</label>
          <input id="gift-expires" type="date" value={expires} onChange={(e) => setExpires(e.target.value)} className={control} />
        </div>
        <div>
          <label htmlFor="gift-note" className="block text-sm font-medium text-gray-700 mb-1.5">Note (optional)</label>
          <input id="gift-note" maxLength={200} value={note} onChange={(e) => setNote(e.target.value)} placeholder="Who it's for" className={control} />
        </div>
      </form>
    </Modal>
  );
}

export function GiftCodesPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.giftCodes);
  const { busy, run } = useAdminAction(reload);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<{ code: string; amount: number } | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  function toggle(code: GiftCode) {
    if (!code.disabled && !confirm(`Disable the code ending ${code.hint}? Nobody can redeem it after this. Credit already added stays.`)) return;
    void run(code.id, () => adminApi.setGiftCodeDisabled(code.id, !code.disabled), code.disabled ? 'Code enabled' : 'Code disabled');
  }

  return (
    <>
      <PageHeader title="Gift codes" description="Codes add credit to the account that redeems them. A code is shown once, when it's made." onRefresh={() => void reload()} loading={loading}>
        <button onClick={() => setCreating(true)} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors whitespace-nowrap">
          <Plus className="w-4 h-4" /> Create code
        </button>
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState icon={<Gift className="w-16 h-16" />} title="No gift codes yet" description="Create one to give someone credit." />
        ) : (
          data && (
            <Table headers={['Code', 'Amount', 'Used', 'Expires', 'Status', 'Created', '']}>
              {data.map((code) => {
                const s = status(code);
                return (
                  <Fragment key={code.id}>
                    <tr>
                      <td className="px-4 py-3 min-w-[9rem]">
                        <code className="text-xs text-gray-900">…{code.hint}</code>
                        {code.note && <div className="text-xs text-gray-500 break-words">{code.note}</div>}
                      </td>
                      <td className="px-4 py-3 tabular-nums text-gray-900 whitespace-nowrap">{formatRupees(code.amount_paise)}</td>
                      <td className="px-4 py-3 tabular-nums whitespace-nowrap">
                        {code.redemptions.length > 0 ? (
                          <button className="text-blue-600 hover:text-blue-700" onClick={() => setOpen(open === code.id ? null : code.id)} aria-expanded={open === code.id}>
                            {code.uses} of {code.max_uses}
                          </button>
                        ) : (
                          <span className="text-gray-700">{code.uses} of {code.max_uses}</span>
                        )}
                      </td>
                      <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{code.expires_at ? formatDate(code.expires_at) : 'Never'}</td>
                      <td className="px-4 py-3"><Badge tone={s.tone}>{s.label}</Badge></td>
                      <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(code.created_at)}</td>
                      <td className="px-4 py-3 text-right">
                        <button className={code.disabled ? secondaryButton : dangerButton} disabled={busy !== null} onClick={() => toggle(code)}>
                          {code.disabled ? 'Enable' : 'Disable'}
                        </button>
                      </td>
                    </tr>
                    {open === code.id && (
                      <tr>
                        <td colSpan={7} className="px-4 pb-4 bg-gray-50">
                          <ul className="pt-3 space-y-1 text-sm">
                            {code.redemptions.map((r) => (
                              <li key={r.user_id} className="flex flex-wrap justify-between gap-2">
                                <span className="text-gray-900 break-all">{r.email ?? r.user_id}</span>
                                <span className="text-gray-500 whitespace-nowrap">{formatDateTime(r.at)}</span>
                              </li>
                            ))}
                          </ul>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </Table>
          )
        )}
      </AdminPageState>

      {creating && (
        <CreateModal
          onClose={() => setCreating(false)}
          onCreated={(code, amount) => {
            setCreating(false);
            setCreated({ code, amount });
            void reload();
          }}
        />
      )}
      {created && (
        <Modal
          title="Save this code now"
          onClose={() => setCreated(null)}
          footer={
            <button onClick={() => setCreated(null)} className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors">
              I've saved it
            </button>
          }
        >
          <div className="space-y-4">
            <p className="text-sm text-amber-800 bg-amber-50 border border-amber-200 rounded-lg px-3 py-2">
              This is the only time the code is shown. It adds {formatRupees(created.amount)} to the account that redeems it.
            </p>
            <CopyField label="Gift code" value={created.code} />
          </div>
        </Modal>
      )}
    </>
  );
}
