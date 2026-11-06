import { FormEvent, useEffect, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { FolderKanban, Mail, Plus } from 'lucide-react';
import { consoleApi, IncomingInvite, Project } from '@/api/console';
import { EmptyState } from '@/components/EmptyState';
import { Modal } from '@/components/Modal';
import { PageState } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatDate } from '@/lib/format';
import { useConsoleStore } from '@/store/useConsoleStore';
import { Badge, errorMessage, useAdminAction, useAdminData } from './admin/shared';

const roleLabel = { owner: 'Owner', member: 'Member' };

function ProjectCard({ project }: { project: Project }) {
  return (
    <Link to={`/projects/${project.id}`} className="block bg-white border border-gray-200 rounded-xl p-5 hover:border-blue-300 hover:shadow-sm transition-all">
      <div className="flex items-start justify-between gap-2 mb-3">
        <h3 className="text-sm font-medium text-gray-900 break-words min-w-0">{project.name}</h3>
        <Badge tone={project.role === 'owner' ? 'blue' : 'gray'}>{roleLabel[project.role]}</Badge>
      </div>
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-gray-400">Created {formatDate(project.created_at)}</p>
        {!project.enabled && <Badge tone="red">Disabled</Badge>}
      </div>
    </Link>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="mb-8">
      <h2 className="text-sm font-medium text-gray-500 mb-3">{title}</h2>
      {children}
    </section>
  );
}

const grid = 'grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4';

function CreateProjectModal({ onClose }: { onClose: () => void }) {
  const [name, setName] = useState('');
  const [creating, setCreating] = useState(false);
  const toast = useToast();
  const navigate = useNavigate();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setCreating(true);
    try {
      const project = await consoleApi.createProject(name.trim());
      navigate(`/projects/${project.id}`);
    } catch (err) {
      toast.error(errorMessage(err, 'Could not create the project'));
      setCreating(false);
    }
  }

  return (
    <Modal
      title="New project"
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
          <button form="create-project-form" type="submit" disabled={creating || !name.trim()} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {creating && <Spinner size="sm" />} Create
          </button>
        </>
      }
    >
      <form id="create-project-form" onSubmit={(e) => void submit(e)}>
        <label htmlFor="project-name" className="block text-sm font-medium text-gray-700 mb-1.5">Project name</label>
        <input
          id="project-name"
          autoFocus
          value={name}
          maxLength={100}
          onChange={(e) => setName(e.target.value)}
          className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
        />
      </form>
    </Modal>
  );
}

export function ProjectsPage() {
  const { data, error, loading, reload } = useAdminData(consoleApi.projects);
  const { busy, run } = useAdminAction(reload);
  const user = useConsoleStore((s) => s.session?.user);
  const setUser = useConsoleStore((s) => s.setUser);
  const [creating, setCreating] = useState(false);
  const canCreate = user?.can_create_projects ?? false;

  // An admin may have approved them since they signed in.
  useEffect(() => {
    consoleApi.me().then(setUser).catch(() => undefined);
  }, [setUser]);

  const invites: IncomingInvite[] = data?.invites ?? [];
  const nothing = data && data.created.length === 0 && data.joined.length === 0 && invites.length === 0;

  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
      <div className="flex items-center justify-between gap-4 mb-6">
        <h1 className="text-xl font-semibold text-gray-900">Projects</h1>
        {canCreate && (
          <button onClick={() => setCreating(true)} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors whitespace-nowrap">
            <Plus className="w-4 h-4" /> New project
          </button>
        )}
      </div>

      <PageState error={error} loading={loading} hasData={data !== null}>
        {nothing && (
          <EmptyState
            icon={<FolderKanban className="w-16 h-16" />}
            title="No projects yet"
            description={
              canCreate
                ? 'Create a project to start storing files, or ask someone to invite you to theirs.'
                : `Ask someone to invite you to their project using ${user?.email ?? 'your email address'}. To create your own, an administrator has to approve your account first.`
            }
            action={
              canCreate ? (
                <button onClick={() => setCreating(true)} className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors">
                  New project
                </button>
              ) : undefined
            }
          />
        )}

        {invites.length > 0 && (
          <Section title="Invitations">
            <div className="space-y-2">
              {invites.map((invite) => (
                <div key={invite.id} className="flex flex-col sm:flex-row sm:items-center gap-3 bg-blue-50 border border-blue-200 rounded-xl px-4 py-3">
                  <Mail className="w-4 h-4 text-blue-600 shrink-0 hidden sm:block" />
                  <p className="text-sm text-gray-800 flex-1 min-w-0 break-words">
                    You're invited to <strong>{invite.project_name}</strong> as {invite.role === 'owner' ? 'an Owner' : 'a Member'}.{' '}
                    <span className="text-gray-500 whitespace-nowrap">Expires {formatDate(invite.expires_at)}</span>
                  </p>
                  <div className="flex gap-2 shrink-0">
                    <button
                      disabled={busy !== null}
                      onClick={() => void run(invite.id, () => consoleApi.declineInvite(invite.id), 'Invitation declined')}
                      className="px-3 py-1.5 text-sm text-gray-600 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50"
                    >
                      Decline
                    </button>
                    <button
                      disabled={busy !== null}
                      onClick={() => void run(invite.id, () => consoleApi.acceptInvite(invite.id), `You joined ${invite.project_name}`)}
                      className="px-3 py-1.5 text-sm font-medium text-white bg-blue-600 rounded-lg hover:bg-blue-700 disabled:opacity-50"
                    >
                      Accept
                    </button>
                  </div>
                </div>
              ))}
            </div>
          </Section>
        )}

        {data && data.created.length > 0 && (
          <Section title="My projects">
            <div className={grid}>{data.created.map((project) => <ProjectCard key={project.id} project={project} />)}</div>
          </Section>
        )}
        {data && data.joined.length > 0 && (
          <Section title="Shared with me">
            <div className={grid}>{data.joined.map((project) => <ProjectCard key={project.id} project={project} />)}</div>
          </Section>
        )}
      </PageState>

      {creating && <CreateProjectModal onClose={() => setCreating(false)} />}
    </div>
  );
}
