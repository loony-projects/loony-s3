import { FormEvent, useState } from 'react';
import { UserPlus } from 'lucide-react';
import { consoleApi, Invite, Member, ProjectRole } from '@/api/console';
import { Modal } from '@/components/Modal';
import { PageState } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatDate } from '@/lib/format';
import { useConsoleStore } from '@/store/useConsoleStore';
import { Badge, PageHeader, Table, dangerButton, errorMessage, secondaryButton, useAdminAction, useAdminData } from '../admin/shared';
import { useProject } from './ProjectLayout';

const roleLabel = { owner: 'Owner', member: 'Member' };

function InviteModal({ projectId, onClose, onInvited }: { projectId: string; onClose: () => void; onInvited: () => void }) {
  const [email, setEmail] = useState('');
  const [role, setRole] = useState<ProjectRole>('member');
  const [sending, setSending] = useState(false);
  const toast = useToast();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setSending(true);
    try {
      await consoleApi.invite(projectId, email.trim(), role);
      toast.success(`Invited ${email.trim()}`);
      onInvited();
    } catch (err) {
      toast.error(errorMessage(err, 'Could not send the invite'));
      setSending(false);
    }
  }

  return (
    <Modal
      title="Invite someone"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
          <button form="invite-form" type="submit" disabled={sending || !email.trim()} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {sending && <Spinner size="sm" />} Invite
          </button>
        </>
      }
    >
      <form id="invite-form" onSubmit={(e) => void submit(e)} className="space-y-4">
        <div>
          <label htmlFor="invite-email" className="block text-sm font-medium text-gray-700 mb-1.5">Email address</label>
          <input
            id="invite-email"
            type="email"
            autoFocus
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
          />
        </div>
        <div>
          <label htmlFor="invite-role" className="block text-sm font-medium text-gray-700 mb-1.5">Role</label>
          <select id="invite-role" value={role} onChange={(e) => setRole(e.target.value as ProjectRole)} className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500">
            <option value="member">Member: can manage buckets and access keys</option>
            <option value="owner">Owner: can also manage members and delete the project</option>
          </select>
        </div>
        <p className="text-xs text-gray-500">
          No email is sent. They'll see the invitation when they sign in with this address, for the next 7 days, so let them know to look for it.
        </p>
      </form>
    </Modal>
  );
}

function PendingInvites({ projectId, refreshKey }: { projectId: string; refreshKey: number }) {
  const { data, error, reload } = useAdminData(() => consoleApi.invites(projectId), [projectId, refreshKey]);
  const { busy, run } = useAdminAction(reload);

  if (error || !data || data.length === 0) return null;

  function cancel(invite: Invite) {
    if (confirm(`Cancel the invitation to ${invite.email}?`)) {
      void run(invite.id, () => consoleApi.cancelInvite(projectId, invite.id), 'Invitation cancelled');
    }
  }

  return (
    <section className="mt-8">
      <h3 className="text-sm font-medium text-gray-900 mb-3">Pending invitations</h3>
      <Table headers={['Email', 'Role', 'Expires', '']}>
        {data.map((invite) => (
          <tr key={invite.id}>
            <td className="px-4 py-3 text-gray-900 break-all">{invite.email}</td>
            <td className="px-4 py-3"><Badge tone="gray">{roleLabel[invite.role]}</Badge></td>
            <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(invite.expires_at)}</td>
            <td className="px-4 py-3 text-right">
              <button className={dangerButton} disabled={busy !== null} onClick={() => cancel(invite)}>Cancel</button>
            </td>
          </tr>
        ))}
      </Table>
    </section>
  );
}

export function MembersPage() {
  const { project, reload: reloadProject } = useProject();
  const { data, error, loading, reload } = useAdminData(() => consoleApi.members(project.id), [project.id]);
  const refresh = async () => {
    await reload();
    await reloadProject();
  };
  const { busy, run } = useAdminAction(refresh);
  const me = useConsoleStore((s) => s.session?.user.id);
  const [inviting, setInviting] = useState(false);
  const [invited, setInvited] = useState(0);

  const isOwner = project.role === 'owner';
  const canManage = isOwner && project.enabled;
  const ownerCount = (data ?? []).filter((member) => member.role === 'owner').length;

  function changeRole(member: Member, role: ProjectRole) {
    const who = member.email ?? 'this person';
    if (member.user_id === me && !confirm('Give up being an Owner of this project? You won\'t be able to undo this yourself.')) return;
    void run(member.user_id, () => consoleApi.setMemberRole(project.id, member.user_id, role), `${who} is now ${role === 'owner' ? 'an Owner' : 'a Member'}`);
  }

  function remove(member: Member) {
    const who = member.email ?? 'this person';
    const warning =
      `Remove ${who} from this project? They lose access straight away.\n\n` +
      "The project's access keys keep working, so replace any they could have copied.";
    if (confirm(warning)) {
      void run(member.user_id, () => consoleApi.removeMember(project.id, member.user_id), `${who} was removed`);
    }
  }

  return (
    <>
      <PageHeader title="Members" description="Everyone here sees this project's buckets and access keys." onRefresh={() => void refresh()} loading={loading}>
        {canManage && (
          <button onClick={() => setInviting(true)} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors whitespace-nowrap">
            <UserPlus className="w-4 h-4" /> Invite
          </button>
        )}
      </PageHeader>

      <PageState error={error} loading={loading} hasData={data !== null}>
        {data && (
          <Table headers={canManage ? ['Person', 'Role', 'Added', ''] : ['Person', 'Role', 'Added']}>
            {data.map((member) => (
              <tr key={member.user_id}>
                <td className="px-4 py-3 min-w-[11rem]">
                  <div className="text-gray-900 break-words">
                    {member.name ?? member.email ?? 'Unknown'} {member.user_id === me && <Badge tone="gray">You</Badge>}
                  </div>
                  {member.name && member.email && <div className="text-xs text-gray-500 break-words">{member.email}</div>}
                </td>
                <td className="px-4 py-3"><Badge tone={member.role === 'owner' ? 'blue' : 'gray'}>{roleLabel[member.role]}</Badge></td>
                <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(member.added_at)}</td>
                {canManage && (
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-2">
                      <button
                        className={`${secondaryButton} whitespace-nowrap`}
                        disabled={busy !== null || (member.role === 'owner' && ownerCount === 1)}
                        title={member.role === 'owner' && ownerCount === 1 ? 'A project needs at least one Owner. Make someone else an Owner first.' : undefined}
                        onClick={() => changeRole(member, member.role === 'owner' ? 'member' : 'owner')}
                      >
                        {member.role === 'owner' ? 'Make Member' : 'Make Owner'}
                      </button>
                      {member.user_id !== me && <button className={dangerButton} disabled={busy !== null} onClick={() => remove(member)}>Remove</button>}
                    </div>
                  </td>
                )}
              </tr>
            ))}
          </Table>
        )}
        {isOwner && <PendingInvites projectId={project.id} refreshKey={invited} />}
      </PageState>

      {inviting && (
        <InviteModal
          projectId={project.id}
          onClose={() => setInviting(false)}
          onInvited={() => {
            setInviting(false);
            setInvited((n) => n + 1);
          }}
        />
      )}
    </>
  );
}
