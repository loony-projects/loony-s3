import { useMemo, useState } from 'react';
import { Users } from 'lucide-react';
import { adminApi, AdminUser } from '@/api/admin';
import { EmptyState } from '@/components/EmptyState';
import { formatDate } from '@/lib/format';
import { AdminPageState, Badge, PageHeader, Table, dangerButton, secondaryButton, useAdminAction, useAdminData } from './shared';

const who = (user: AdminUser) => user.email ?? user.name ?? user.id;

export function UsersPage() {
  const { data, error, loading, reload } = useAdminData(adminApi.users);
  const { busy, run } = useAdminAction(reload);
  const [filter, setFilter] = useState('');

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (data ?? []).filter((u) => (u.email ?? '').toLowerCase().includes(needle) || (u.name ?? '').toLowerCase().includes(needle));
  }, [data, filter]);

  function toggleEnabled(user: AdminUser) {
    const warning =
      `Disable ${who(user)}? They're signed out of everything straight away and can't sign back in.\n\n` +
      'Their projects keep running, and access keys in those projects keep working. Disable the project to stop those.';
    if (user.enabled && !confirm(warning)) return;
    void run(user.id, () => adminApi.updateUser(user.id, { enabled: !user.enabled }), user.enabled ? `${who(user)} was disabled` : `${who(user)} was enabled`);
  }

  return (
    <>
      <PageHeader
        title="Users"
        description="Everyone who has signed in with their Loony account. Their passwords are managed by loony-auth, not here."
        onRefresh={() => void reload()}
        loading={loading}
      >
        <input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter by name or email"
          aria-label="Filter users by name or email"
          className="min-w-0 px-3 py-2 text-sm border border-gray-200 rounded-lg bg-white focus:outline-none focus:ring-2 focus:ring-blue-500"
        />
      </PageHeader>
      <AdminPageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 ? (
          <EmptyState icon={<Users className="w-16 h-16" />} title="No users yet" description="People appear here after they first sign in." />
        ) : (
          data && (
            <Table headers={['Person', 'Status', 'Last sign-in', '']}>
              {visible.map((user) => (
                <tr key={user.id}>
                  <td className="px-4 py-3 min-w-[12rem]">
                    <div className="text-gray-900 break-words">{user.name ?? user.email ?? user.id}</div>
                    {user.name && user.email && <div className="text-xs text-gray-500 break-words">{user.email}</div>}
                    {!user.email_verified && <div className="mt-1"><Badge tone="amber">Email not verified</Badge></div>}
                  </td>
                  <td className="px-4 py-3"><Badge tone={user.enabled ? 'green' : 'red'}>{user.enabled ? 'Enabled' : 'Disabled'}</Badge></td>
                  <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(user.last_sign_in_at)}</td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-2">
                      <button className={user.enabled ? dangerButton : secondaryButton} disabled={busy !== null} onClick={() => toggleEnabled(user)}>
                        {user.enabled ? 'Disable' : 'Enable'}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
              {visible.length === 0 && (
                <tr><td colSpan={4} className="px-4 py-8 text-center text-gray-500">No users match that filter.</td></tr>
              )}
            </Table>
          )
        )}
      </AdminPageState>
    </>
  );
}
