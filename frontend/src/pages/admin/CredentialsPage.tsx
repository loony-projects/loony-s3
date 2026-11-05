import { FormEvent, useState } from 'react';
import { Check, Copy, Plus } from 'lucide-react';
import { adminApi, AdminCredential, CreatedCredential } from '@/api/admin';
import { Modal } from '@/components/Modal';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatDate } from '@/lib/format';
import { useAuthStore } from '@/store/useAuthStore';
import { AdminPageState, Badge, PageHeader, ShortId, Table, dangerButton, errorMessage, secondaryButton, useAdminAction, useAdminData } from './shared';

function CopyField({ label, value }: { label: string; value: string }) {
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

function CreateKeyModal({ owners, onClose, onCreated }: { owners: string[]; onClose: () => void; onCreated: (c: CreatedCredential) => void }) {
  const [owner, setOwner] = useState('');
  const [creating, setCreating] = useState(false);
  const toast = useToast();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setCreating(true);
    try {
      onCreated(await adminApi.createCredential(owner || undefined));
    } catch (err) {
      toast.error(errorMessage(err, 'Could not create the key'));
    } finally {
      setCreating(false);
    }
  }

  return (
    <Modal
      title="Create access key"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
          <button form="create-key-form" type="submit" disabled={creating} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {creating && <Spinner size="sm" />} Create
          </button>
        </>
      }
    >
      <form id="create-key-form" onSubmit={(e) => void submit(e)}>
        <label htmlFor="key-owner" className="block text-sm font-medium text-gray-700 mb-1.5">Owner</label>
        <select id="key-owner" value={owner} onChange={(e) => setOwner(e.target.value)} className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500">
          <option value="">New owner (separate, empty set of buckets)</option>
          {owners.map((id) => <option key={id} value={id}>Existing owner {id}</option>)}
        </select>
        <p className="mt-2 text-xs text-gray-500">
          A key can reach its owner's buckets and nothing else. Keys created here never get admin access.
        </p>
      </form>
    </Modal>
  );
}

export function CredentialsPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.credentials);
  const { busy, run } = useAdminAction(reload);
  const signedInKey = useAuthStore((s) => s.accessKey);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedCredential | null>(null);

  const owners = [...new Set((data ?? []).map((c) => c.owner_id))];

  function remove(cred: AdminCredential) {
    if (confirm(`Delete access key ${cred.access_key}? Anything using it stops working immediately. This can't be undone.`)) {
      void run(cred.access_key, () => adminApi.deleteCredential(cred.access_key), 'Key deleted');
    }
  }

  function toggle(cred: AdminCredential) {
    if (cred.enabled && !confirm(`Disable access key ${cred.access_key}? Anything using it stops working until it's enabled again.`)) return;
    void run(
      cred.access_key,
      () => adminApi.setCredentialEnabled(cred.access_key, !cred.enabled),
      cred.enabled ? 'Key disabled' : 'Key enabled',
    );
  }

  return (
    <>
      <PageHeader title="Access keys" description="Secrets are shown once, when a key is created, and can't be retrieved afterwards." onRefresh={() => void reload()} loading={loading}>
        <button onClick={() => setCreating(true)} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors">
          <Plus className="w-4 h-4" /> Create key
        </button>
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && (
          <Table headers={['Access key', 'Owner', 'Status', 'Created', '']}>
            {data.map((cred) => {
              const isOwnKey = cred.access_key === signedInKey;
              const ownKeyHint = isOwnKey ? "You're signed in with this key" : undefined;
              return (
                <tr key={cred.access_key}>
                  <td className="px-4 py-3">
                    <div className="flex items-center gap-2">
                      <code className="text-xs text-gray-900">{cred.access_key}</code>
                      {cred.admin && <Badge tone="blue">Admin</Badge>}
                      {isOwnKey && <Badge tone="gray">You</Badge>}
                    </div>
                  </td>
                  <td className="px-4 py-3"><ShortId value={cred.owner_id} /></td>
                  <td className="px-4 py-3"><Badge tone={cred.enabled ? 'green' : 'gray'}>{cred.enabled ? 'Enabled' : 'Disabled'}</Badge></td>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(cred.created_at)}</td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-2">
                      <button className={secondaryButton} disabled={busy !== null || isOwnKey} title={ownKeyHint} onClick={() => toggle(cred)}>
                        {cred.enabled ? 'Disable' : 'Enable'}
                      </button>
                      <button className={dangerButton} disabled={busy !== null || isOwnKey} title={ownKeyHint} onClick={() => remove(cred)}>
                        Delete
                      </button>
                    </div>
                  </td>
                </tr>
              );
            })}
          </Table>
        )}
      </AdminPageState>

      {creating && (
        <CreateKeyModal
          owners={owners}
          onClose={() => setCreating(false)}
          onCreated={(c) => {
            setCreating(false);
            setCreated(c);
            void reload();
          }}
        />
      )}

      {created && (
        <Modal
          title="Save this secret now"
          size="lg"
          onClose={() => setCreated(null)}
          footer={
            <button onClick={() => setCreated(null)} className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors">
              I've saved it
            </button>
          }
        >
          <div className="space-y-4">
            <p className="text-sm text-amber-800 bg-amber-50 border border-amber-200 rounded-lg px-3 py-2">
              This is the only time the secret key is shown. If you lose it, delete this key and create another.
            </p>
            <CopyField label="Access key" value={created.access_key} />
            <CopyField label="Secret key" value={created.secret_key} />
          </div>
        </Modal>
      )}
    </>
  );
}
