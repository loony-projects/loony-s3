import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import { NavLink, Navigate, Route, Routes, useNavigate, useParams } from 'react-router-dom';
import { FolderOpen, KeyRound, Settings, Users } from 'lucide-react';
import { ApiError } from '@/api/client';
import { consoleApi, Project } from '@/api/console';
import { PageError } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { BucketPage } from '@/pages/BucketPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { useConsoleStore } from '@/store/useConsoleStore';
import { Badge, useAdminData } from '../admin/shared';
import { AccessKeysPage } from './AccessKeysPage';
import { MembersPage } from './MembersPage';
import { SettingsPage } from './SettingsPage';

interface ProjectContextValue {
  project: Project;
  reload: () => Promise<void>;
}

const ProjectContext = createContext<ProjectContextValue | null>(null);

export function useProject(): ProjectContextValue {
  const value = useContext(ProjectContext);
  if (!value) throw new Error('useProject outside a project');
  return value;
}

/** Fetches this project's key for the bucket pages, which sign with it. */
function BucketArea({ project }: { project: Project }) {
  const projectKey = useConsoleStore((s) => s.projectKey);
  const setProjectKey = useConsoleStore((s) => s.setProjectKey);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!project.enabled) return;
    let current = true;
    setError(null);
    consoleApi
      .openProject(project.id)
      .then((opened) => {
        if (current) setProjectKey({ projectId: project.id, accessKey: opened.access_key, secretKey: opened.secret_key });
      })
      .catch((err: unknown) => {
        if (current) setError(err instanceof ApiError ? err.message : 'Could not open this project.');
      });
    return () => {
      current = false;
      setProjectKey(null);
    };
  }, [project.id, project.enabled, setProjectKey]);

  if (!project.enabled) {
    return <PageError title="This project is disabled">An administrator has disabled this project, so its buckets and access keys don't work right now.</PageError>;
  }
  if (error) return <PageError title="Couldn't open this project">{error}</PageError>;
  if (projectKey?.projectId !== project.id) {
    return <div className="flex justify-center py-20"><Spinner size="lg" /></div>;
  }
  return (
    <Routes>
      <Route index element={<DashboardPage />} />
      <Route path=":name" element={<BucketPage />} />
    </Routes>
  );
}

const tabs = [
  { to: 'buckets', label: 'Buckets', icon: FolderOpen },
  { to: 'keys', label: 'Access keys', icon: KeyRound },
  { to: 'members', label: 'Members', icon: Users },
  { to: 'settings', label: 'Settings', icon: Settings },
];

function ProjectSwitcher({ project }: { project: Project }) {
  const navigate = useNavigate();
  const { data } = useAdminData(consoleApi.projects);
  const others = useMemo(() => [...(data?.created ?? []), ...(data?.joined ?? [])].filter((p) => p.id !== project.id), [data, project.id]);
  // This project comes from the page, so a rename shows up straight away.
  const options = [project, ...others];

  return (
    <select
      aria-label="Switch project"
      value={project.id}
      onChange={(e) => navigate(`/projects/${e.target.value}`)}
      className="max-w-full min-w-0 text-xl font-semibold text-gray-900 bg-transparent border-0 rounded-lg py-1 pl-1 pr-8 focus:outline-none focus:ring-2 focus:ring-blue-500 truncate"
    >
      {options.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
    </select>
  );
}

export function ProjectLayout() {
  const { projectId } = useParams();
  const [project, setProject] = useState<Project | null>(null);
  const [error, setError] = useState<unknown>(null);

  const reload = useCallback(async () => {
    if (!projectId) return;
    try {
      setProject(await consoleApi.project(projectId));
      setError(null);
    } catch (err) {
      setError(err);
    }
  }, [projectId]);

  useEffect(() => {
    setProject(null);
    setError(null);
    void reload();
  }, [reload]);

  const contextValue = useMemo(() => (project ? { project, reload } : null), [project, reload]);

  const shell = 'max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8';
  if (error) {
    const missing = error instanceof ApiError && error.status === 404;
    return (
      <div className={shell}>
        <PageError title={missing ? "We can't find that project" : "That didn't load"}>
          {missing ? "It doesn't exist, or you don't have access to it." : error instanceof ApiError ? error.message : 'The request failed.'}
        </PageError>
      </div>
    );
  }
  if (!project || !contextValue || project.id !== projectId) {
    return <div className="flex justify-center py-20"><Spinner size="lg" /></div>;
  }

  return (
    <ProjectContext.Provider value={contextValue}>
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 pt-6">
        <div className="flex flex-wrap items-center gap-2 mb-3">
          <ProjectSwitcher project={project} />
          <Badge tone={project.role === 'owner' ? 'blue' : 'gray'}>{project.role === 'owner' ? 'Owner' : 'Member'}</Badge>
          {!project.enabled && <Badge tone="red">Disabled</Badge>}
        </div>
        <nav aria-label="Project sections" className="flex gap-1 border-b border-gray-200 overflow-x-auto">
          {tabs.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={`/projects/${project.id}/${to}`}
              className={({ isActive }) =>
                `flex items-center gap-2 px-3 py-2.5 text-sm font-medium whitespace-nowrap border-b-2 -mb-px transition-colors ${
                  isActive ? 'border-blue-600 text-blue-600' : 'border-transparent text-gray-500 hover:text-gray-800'
                }`
              }
            >
              <Icon className="w-4 h-4" />
              {label}
            </NavLink>
          ))}
        </nav>
      </div>
      <Routes>
        <Route index element={<Navigate to="buckets" replace />} />
        <Route path="buckets/*" element={<BucketArea project={project} />} />
        <Route path="keys" element={<div className={shell}><AccessKeysPage /></div>} />
        <Route path="members" element={<div className={shell}><MembersPage /></div>} />
        <Route path="settings" element={<div className={shell}><SettingsPage /></div>} />
        <Route path="*" element={<Navigate to="buckets" replace />} />
      </Routes>
    </ProjectContext.Provider>
  );
}
