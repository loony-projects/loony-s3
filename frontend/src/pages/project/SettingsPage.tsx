import { FormEvent, useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { consoleApi } from '@/api/console';
import { Modal } from '@/components/Modal';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { errorMessage } from '../admin/shared';
import { useProject } from './ProjectLayout';

function Card({ title, description, children }: { title: string; description: string; children: React.ReactNode }) {
  return (
    <section className="bg-white border border-gray-200 rounded-xl p-5">
      <h3 className="text-sm font-medium text-gray-900">{title}</h3>
      <p className="text-sm text-gray-500 mt-0.5 mb-4">{description}</p>
      {children}
    </section>
  );
}

export function SettingsPage() {
  const { project, reload } = useProject();
  const navigate = useNavigate();
  const toast = useToast();
  const [name, setName] = useState(project.name);
  const [saving, setSaving] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [confirmation, setConfirmation] = useState('');
  const [deleting, setDeleting] = useState(false);

  useEffect(() => setName(project.name), [project.name]);

  const isOwner = project.role === 'owner';
  const canRename = isOwner && project.enabled;

  async function rename(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    try {
      await consoleApi.renameProject(project.id, name.trim());
      await reload();
      toast.success('Project renamed');
    } catch (err) {
      toast.error(errorMessage(err, 'Could not rename the project'));
    } finally {
      setSaving(false);
    }
  }

  async function leave() {
    if (!confirm(`Leave ${project.name}? You'll lose access unless someone invites you again.`)) return;
    setLeaving(true);
    try {
      await consoleApi.leaveProject(project.id);
      toast.success(`You left ${project.name}`);
      navigate('/');
    } catch (err) {
      toast.error(errorMessage(err, 'Could not leave the project'));
      setLeaving(false);
    }
  }

  async function remove() {
    setDeleting(true);
    try {
      await consoleApi.deleteProject(project.id);
      toast.success(`${project.name} was deleted`);
      navigate('/');
    } catch (err) {
      toast.error(errorMessage(err, 'Could not delete the project'));
      setDeleting(false);
      setConfirmingDelete(false);
    }
  }

  return (
    <div className="max-w-2xl space-y-5">
      <h2 className="text-lg font-semibold text-gray-900">Settings</h2>

      {canRename && (
        <Card title="Project name" description="Everyone in the project sees this name.">
          <form onSubmit={(e) => void rename(e)} className="flex flex-col sm:flex-row gap-2">
            <label htmlFor="rename-project" className="sr-only">Project name</label>
            <input
              id="rename-project"
              value={name}
              maxLength={100}
              onChange={(e) => setName(e.target.value)}
              className="flex-1 min-w-0 px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
            <button type="submit" disabled={saving || !name.trim() || name.trim() === project.name} className="flex items-center justify-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
              {saving && <Spinner size="sm" />} Save
            </button>
          </form>
        </Card>
      )}

      <Card title="Leave this project" description="You'll lose access. The project's buckets and access keys stay as they are. A project must keep at least one Owner.">
        <button onClick={() => void leave()} disabled={leaving} className="px-4 py-2 text-sm font-medium text-gray-700 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 disabled:opacity-50 transition-colors">
          Leave project
        </button>
      </Card>

      {isOwner && (
        <Card title="Delete this project" description="Removes its access keys, members and invitations for good. Delete its buckets first: a project with buckets can't be deleted.">
          <button onClick={() => setConfirmingDelete(true)} className="px-4 py-2 text-sm font-medium text-red-700 bg-white border border-red-200 rounded-lg hover:bg-red-50 transition-colors">
            Delete project
          </button>
        </Card>
      )}

      {confirmingDelete && (
        <Modal
          title="Delete this project?"
          onClose={() => setConfirmingDelete(false)}
          footer={
            <>
              <button onClick={() => setConfirmingDelete(false)} className="px-3 py-2 text-sm text-gray-600 hover:text-gray-800 transition-colors">Cancel</button>
              <button onClick={() => void remove()} disabled={deleting || confirmation !== project.name} className="flex items-center gap-2 px-4 py-2 bg-red-600 hover:bg-red-700 disabled:bg-red-300 text-white text-sm font-medium rounded-lg transition-colors">
                {deleting && <Spinner size="sm" />} Delete project
              </button>
            </>
          }
        >
          <label htmlFor="confirm-delete" className="block text-sm text-gray-700 mb-2">
            This can't be undone. Anything using this project's access keys stops working. Type <strong className="break-all">{project.name}</strong> to confirm.
          </label>
          <input
            id="confirm-delete"
            autoFocus
            value={confirmation}
            onChange={(e) => setConfirmation(e.target.value)}
            className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-red-500"
          />
        </Modal>
      )}
    </div>
  );
}
