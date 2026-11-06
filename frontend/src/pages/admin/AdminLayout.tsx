import { NavLink, Navigate, Route, Routes } from 'react-router-dom';
import { Activity, FolderKanban, FolderOpen, KeyRound, ScrollText, Server, Users } from 'lucide-react';
import { OverviewPage } from './OverviewPage';
import { NodesPage } from './NodesPage';
import { BucketsPage } from './BucketsPage';
import { BucketContentsPage } from './BucketContentsPage';
import { CredentialsPage } from './CredentialsPage';
import { AuditPage } from './AuditPage';
import { ProjectsPage } from './ProjectsPage';
import { UsersPage } from './UsersPage';

const sections = [
  { to: '/admin/overview', label: 'Overview', icon: Activity },
  { to: '/admin/nodes', label: 'Nodes', icon: Server },
  { to: '/admin/users', label: 'Users', icon: Users },
  { to: '/admin/projects', label: 'Projects', icon: FolderKanban },
  { to: '/admin/buckets', label: 'All buckets', icon: FolderOpen },
  { to: '/admin/keys', label: 'Access keys', icon: KeyRound },
  { to: '/admin/audit', label: 'Audit log', icon: ScrollText },
];

export function AdminLayout() {
  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
      <h1 className="text-xl font-semibold text-gray-900 mb-4">Admin</h1>
      <nav aria-label="Admin sections" className="flex gap-1 border-b border-gray-200 mb-6 overflow-x-auto">
        {sections.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
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
      <Routes>
        <Route path="overview" element={<OverviewPage />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="users" element={<UsersPage />} />
        <Route path="projects" element={<ProjectsPage />} />
        <Route path="buckets" element={<BucketsPage />} />
        <Route path="buckets/:name" element={<BucketContentsPage />} />
        <Route path="keys" element={<CredentialsPage />} />
        <Route path="audit" element={<AuditPage />} />
        <Route path="*" element={<Navigate to="/admin/overview" replace />} />
      </Routes>
    </div>
  );
}
