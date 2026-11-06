import { FormEvent, useState } from 'react';
import { FolderKanban } from 'lucide-react';
import { adminApi, AdminProject, AdoptableOwner } from '@/api/admin';
import { EmptyState } from '@/components/EmptyState';
import { Modal } from '@/components/Modal';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatBytes, formatDate } from '@/lib/format';
import { AdminPageState, Badge, PageHeader, ShortId, Table, dangerButton, errorMessage, secondaryButton, useAdminAction, useAdminData } from './shared';

const count = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;

const person = (member: AdminProject['members'][number]) => member.email ?? member.name ?? member.user_id;

function AdoptModal({ owners, onClose, onAdopted }: { owners: AdoptableOwner[]; onClose: () => void; onAdopted: () => void }) {
  const users = useAdminData(adminApi.users);
  const [ownerId, setOwnerId] = useState(owners[0]?.owner_id ?? '');
  const [userId, setUserId] = useState('');
  const [name, setName] = useState('');
  const [saving, setSaving] = useState(false);
  const toast = useToast();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    try {
      const project = await adminApi.adoptOwner(ownerId, name.trim(), userId);
      toast.success(`${project.name} was created`);
      onAdopted();
    } catch (err) {
      toast.error(errorMessage(err, 'Could not move this account'));
      setSaving(false);
    }
  }

  const control = 'w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500';

  return (
    <Modal
      title="Move an account into a project"
      size="lg"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
          <button form="adopt-form" type="submit" disabled={saving || !ownerId || !userId || !name.trim()} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {saving && <Spinner size="sm" />} Move into project
          </button>
        </>
      }
    >
      <form id="adopt-form" onSubmit={(e) => void submit(e)} className="space-y-4">
        <div>
          <label htmlFor="adopt-owner" className="block text-sm font-medium text-gray-700 mb-1.5">Account</label>
          <select id="adopt-owner" value={ownerId} onChange={(e) => setOwnerId(e.target.value)} className={control}>
            {owners.map((owner) => (
              <option key={owner.owner_id} value={owner.owner_id}>
                {owner.owner_id.slice(0, 8)}… ({count(owner.access_keys, 'key')}, {count(owner.buckets, 'bucket')}, {formatBytes(owner.bytes)})
              </option>
            ))}
          </select>
        </div>
        <div>
          <label htmlFor="adopt-name" className="block text-sm font-medium text-gray-700 mb-1.5">Project name</label>
          <input id="adopt-name" value={name} maxLength={100} onChange={(e) => setName(e.target.value)} className={control} />
        </div>
        <div>
          <label htmlFor="adopt-user" className="block text-sm font-medium text-gray-700 mb-1.5">Project Owner</label>
          <select id="adopt-user" value={userId} onChange={(e) => setUserId(e.target.value)} className={control}>
            <option value="">Choose someone</option>
            {(users.data ?? []).filter((user) => user.enabled).map((user) => (
              <option key={user.id} value={user.id}>{user.email ?? user.name ?? user.id}</option>
            ))}
          </select>
          <p className="mt-1.5 text-xs text-gray-500">Only people who have signed in at least once are listed.</p>
        </div>
        <p className="text-xs text-gray-500">
          The account's buckets and keys move into the project, and apps using those keys keep working. This can't be undone from here.
        </p>
      </form>
    </Modal>
  );
}

export function ProjectsPage() {
  const { data, error, loading, reload: reloadProjects } = useAdminData(adminApi.projects);
  const owners = useAdminData(adminApi.adoptableOwners);
  const reload = async () => {
    await reloadProjects();
    await owners.reload();
  };
  const { busy, run } = useAdminAction(reload);
  const [adopting, setAdopting] = useState(false);

  function toggle(project: AdminProject) {
    const warning =
      `Disable ${project.name}? All its access keys stop working straight away, and its members can't work in it.\n\n` +
      'Nothing is deleted. Enabling it again restores its keys.';
    if (project.enabled && !confirm(warning)) return;
    void run(project.id, () => adminApi.setProjectEnabled(project.id, !project.enabled), project.enabled ? `${project.name} was disabled` : `${project.name} was enabled`);
  }

  const canAdopt = (owners.data?.length ?? 0) > 0;

  return (
    <>
      <PageHeader title="Projects" description="Every project, whoever is in it." onRefresh={() => void reload()} loading={loading}>
        {canAdopt && (
          <button onClick={() => setAdopting(true)} className="px-4 py-2 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors whitespace-nowrap">
            Move an account into a project
          </button>
        )}
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState icon={<FolderKanban className="w-16 h-16" />} title="No projects yet" description="Approve someone under Users, and they can create one." />
        ) : (
          data && (
            <Table headers={['Project', 'Status', 'Members', 'Access keys', 'Buckets', 'Objects', 'Size', 'Created', '']}>
              {data.map((project) => (
                <tr key={project.id}>
                  <td className="px-4 py-3 min-w-[10rem]">
                    <div className="font-medium text-gray-900 break-words">{project.name}</div>
                    <ShortId value={project.owner_id} />
                  </td>
                  <td className="px-4 py-3"><Badge tone={project.enabled ? 'green' : 'red'}>{project.enabled ? 'Enabled' : 'Disabled'}</Badge></td>
                  <td className="px-4 py-3 min-w-[12rem]">
                    {project.members.map((member) => (
                      <div key={member.user_id} className="text-gray-700 break-words">
                        {person(member)} {member.role === 'owner' && <span className="text-xs text-gray-500">(Owner)</span>}
                      </div>
                    ))}
                  </td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{project.access_keys.toLocaleString()}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{project.buckets.toLocaleString()}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700">{project.objects.toLocaleString()}</td>
                  <td className="px-4 py-3 tabular-nums text-gray-700 whitespace-nowrap">{formatBytes(project.bytes)}</td>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(project.created_at)}</td>
                  <td className="px-4 py-3 text-right">
                    <button className={project.enabled ? dangerButton : secondaryButton} disabled={busy !== null} onClick={() => toggle(project)}>
                      {project.enabled ? 'Disable' : 'Enable'}
                    </button>
                  </td>
                </tr>
              ))}
            </Table>
          )
        )}
      </AdminPageState>

      {adopting && owners.data && (
        <AdoptModal
          owners={owners.data}
          onClose={() => setAdopting(false)}
          onAdopted={() => {
            setAdopting(false);
            void reload();
          }}
        />
      )}
    </>
  );
}
