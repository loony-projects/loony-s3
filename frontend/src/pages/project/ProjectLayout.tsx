import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import { NavLink, Navigate, Route, Routes, useLocation, useNavigate, useParams } from 'react-router-dom';
import { FolderOpen, Gauge, KeyRound, Settings, Users } from 'lucide-react';
import { ApiError } from '@/api/client';
import { consoleApi, Project, Subscription } from '@/api/console';
import { SubscriptionBanner } from '@/components/SubscriptionPanel';
import { formatUsage } from '@/lib/format';
import { PageError } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { BucketPage } from '@/pages/BucketPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { useConsoleStore } from '@/store/useConsoleStore';
import { Badge, useAdminData } from '../admin/shared';
import { AccessKeysPage } from './AccessKeysPage';
import { MembersPage } from './MembersPage';
import { PlanPage } from './PlanPage';
import { SettingsPage } from './SettingsPage';

interface ProjectContextValue {
  project: Project;
  subscription: Subscription;
  reload: () => Promise<void>;
}

const ProjectContext = createContext<ProjectContextValue | null>(null);

export function useProject(): ProjectContextValue {
  const value = useContext(ProjectContext);
  if (!value) throw new Error('useProject outside a project');
  return value;
}

const pageShell = 'max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8';

/** Fetches this project's key for the bucket pages, which sign with it. */
function BucketArea({ project, subscription }: { project: Project; subscription: Subscription }) {
  const projectKey = useConsoleStore((s) => s.projectKey);
  const setProjectKey = useConsoleStore((s) => s.setProjectKey);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!project.enabled || subscription.state === 'unpaid') return;
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
  }, [project.id, project.enabled, subscription.state, setProjectKey]);

  if (!project.enabled) {
    return (
      <div className={pageShell}>
        <PageError title="This project is disabled">An administrator has disabled this project, so its buckets and access keys don't work right now.</PageError>
      </div>
    );
  }
  if (subscription.state === 'unpaid') {
    return (
      <div className={pageShell}>
        <PageError title="Nothing here yet">
          This project isn't paid for, so it can't hold buckets.{' '}
          {project.role === 'owner' ? (
            <NavLink to={`/projects/${project.id}/plan`} className="font-medium underline underline-offset-2">Subscribe to start</NavLink>
          ) : (
            'Ask a project Owner to subscribe.'
          )}
        </PageError>
      </div>
    );
  }
  if (error) {
    return (
      <div className={pageShell}>
        <PageError title="Couldn't open this project">{error}</PageError>
      </div>
    );
  }
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
  { to: 'plan', label: 'Plan', icon: Gauge },
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
  const [subscription, setSubscription] = useState<Subscription | null>(null);
  const [error, setError] = useState<unknown>(null);

  const reload = useCallback(async () => {
    if (!projectId) return;
    try {
      const [loaded, sub] = await Promise.all([consoleApi.project(projectId), consoleApi.subscription(projectId)]);
      setProject(loaded);
      setSubscription(sub);
      setError(null);
    } catch (err) {
      setError(err);
    }
  }, [projectId]);

  useEffect(() => {
    setProject(null);
    setSubscription(null);
    setError(null);
    void reload();
  }, [reload]);

  // Usage and state change as people upload and pay, so refresh them on each page change.
  const location = useLocation();
  useEffect(() => {
    if (!projectId) return;
    consoleApi.subscription(projectId).then(setSubscription).catch(() => undefined);
  }, [projectId, location.pathname]);

  const contextValue = useMemo(
    () => (project && subscription ? { project, subscription, reload } : null),
    [project, subscription, reload],
  );

  if (error) {
    const missing = error instanceof ApiError && error.status === 404;
    return (
      <div className={pageShell}>
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
          {subscription && subscription.state !== 'unpaid' && (
            <span className="ml-auto text-sm text-gray-500 whitespace-nowrap">
              {formatUsage(subscription.usage.bytes + subscription.usage.pending_bytes, subscription.usage.limit_bytes)}
            </span>
          )}
        </div>
        {subscription && project.enabled && (
          <div className="mb-3">
            <SubscriptionBanner subscription={subscription} projectId={project.id} isOwner={project.role === 'owner'} />
          </div>
        )}
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
        <Route path="buckets/*" element={<BucketArea project={project} subscription={contextValue.subscription} />} />
        <Route path="plan" element={<div className={pageShell}><PlanPage /></div>} />
        <Route path="keys" element={<div className={pageShell}><AccessKeysPage /></div>} />
        <Route path="members" element={<div className={pageShell}><MembersPage /></div>} />
        <Route path="settings" element={<div className={pageShell}><SettingsPage /></div>} />
        <Route path="*" element={<Navigate to="buckets" replace />} />
      </Routes>
    </ProjectContext.Provider>
  );
}
