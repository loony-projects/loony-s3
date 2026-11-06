import { FormEvent, useState } from 'react';
import { KeyRound, Plus } from 'lucide-react';
import { AccessKey, consoleApi, CreatedAccessKey } from '@/api/console';
import { CopyField } from '@/components/CopyField';
import { EmptyState } from '@/components/EmptyState';
import { Modal } from '@/components/Modal';
import { PageState } from '@/components/PageError';
import { Spinner } from '@/components/Spinner';
import { useToast } from '@/context/ToastContext';
import { formatDate } from '@/lib/format';
import { API_ENDPOINT, API_REGION } from '@/store/useConsoleStore';
import { Badge, PageHeader, Table, dangerButton, errorMessage, secondaryButton, useAdminAction, useAdminData } from '../admin/shared';
import { useProject } from './ProjectLayout';

type Language = 'python' | 'javascript' | 'rclone';

function example(language: Language, accessKey: string, secretKey: string): string {
  switch (language) {
    case 'python':
      return `import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="${API_ENDPOINT}",
    region_name="${API_REGION}",
    aws_access_key_id="${accessKey}",
    aws_secret_access_key="${secretKey}",
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)

s3.upload_file("photo.jpg", "my-bucket", "photo.jpg")`;
    case 'javascript':
      return `import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";

const s3 = new S3Client({
  endpoint: "${API_ENDPOINT}",
  region: "${API_REGION}",
  forcePathStyle: true,
  credentials: {
    accessKeyId: "${accessKey}",
    secretAccessKey: "${secretKey}",
  },
});

await s3.send(new PutObjectCommand({ Bucket: "my-bucket", Key: "hello.txt", Body: "Hello" }));`;
    case 'rclone':
      return `export RCLONE_CONFIG_LOONY_TYPE=s3
export RCLONE_CONFIG_LOONY_PROVIDER=Other
export RCLONE_CONFIG_LOONY_LIST_VERSION=2
export RCLONE_CONFIG_LOONY_ENDPOINT=${API_ENDPOINT}
export RCLONE_CONFIG_LOONY_REGION=${API_REGION}
export RCLONE_CONFIG_LOONY_ACCESS_KEY_ID=${accessKey}
export RCLONE_CONFIG_LOONY_SECRET_ACCESS_KEY=${secretKey}

rclone copy ./photo.jpg loony:my-bucket/`;
  }
}

function ConnectionDetails({ accessKey, secretKey }: { accessKey: string; secretKey: string }) {
  const [language, setLanguage] = useState<Language>('python');
  const toast = useToast();
  const text = example(language, accessKey, secretKey);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      toast.success('Example copied');
    } catch {
      toast.error('Could not copy. Select the text and copy it manually.');
    }
  }

  return (
    <div>
      <div className="flex items-center justify-between gap-2 mb-2">
        <div role="tablist" aria-label="Example language" className="flex gap-1">
          {(['python', 'javascript', 'rclone'] as const).map((option) => (
            <button
              key={option}
              role="tab"
              aria-selected={language === option}
              onClick={() => setLanguage(option)}
              className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors ${language === option ? 'bg-blue-50 text-blue-700' : 'text-gray-500 hover:text-gray-800'}`}
            >
              {{ python: 'Python', javascript: 'JavaScript', rclone: 'rclone' }[option]}
            </button>
          ))}
        </div>
        <button onClick={() => void copy()} className="text-xs font-medium text-blue-600 hover:text-blue-700">Copy</button>
      </div>
      <pre className="text-xs bg-gray-50 border border-gray-200 rounded-lg p-3 overflow-x-auto max-h-64">{text}</pre>
    </div>
  );
}

function CreateKeyModal({ projectId, onClose, onCreated }: { projectId: string; onClose: () => void; onCreated: (key: CreatedAccessKey) => void }) {
  const [name, setName] = useState('');
  const [creating, setCreating] = useState(false);
  const toast = useToast();

  async function submit(e: FormEvent) {
    e.preventDefault();
    setCreating(true);
    try {
      onCreated(await consoleApi.createAccessKey(projectId, name.trim()));
    } catch (err) {
      toast.error(errorMessage(err, 'Could not create the key'));
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
          <button form="create-access-key" type="submit" disabled={creating || !name.trim()} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-300 text-white text-sm font-medium rounded-lg transition-colors">
            {creating && <Spinner size="sm" />} Create
          </button>
        </>
      }
    >
      <form id="create-access-key" onSubmit={(e) => void submit(e)}>
        <label htmlFor="key-name" className="block text-sm font-medium text-gray-700 mb-1.5">Key name</label>
        <input
          id="key-name"
          autoFocus
          value={name}
          maxLength={100}
          placeholder="Production server"
          onChange={(e) => setName(e.target.value)}
          className="w-full px-3 py-2 text-sm border border-gray-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-blue-500"
        />
        <p className="mt-2 text-xs text-gray-500">
          Apps use this key to reach this project's buckets. It belongs to the project, so it keeps working if you leave.
        </p>
      </form>
    </Modal>
  );
}

export function AccessKeysPage() {
  const { project } = useProject();
  const { data, error, loading, reload } = useAdminData(() => consoleApi.accessKeys(project.id), [project.id]);
  const { busy, run } = useAdminAction(reload);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedAccessKey | null>(null);

  function remove(key: AccessKey) {
    if (confirm(`Delete "${key.name ?? key.access_key}"? Anything using it stops working immediately. This can't be undone.`)) {
      void run(key.access_key, () => consoleApi.deleteAccessKey(project.id, key.access_key), 'Key deleted');
    }
  }

  function toggle(key: AccessKey) {
    if (key.enabled && !confirm(`Disable "${key.name ?? key.access_key}"? Anything using it stops working until it's enabled again.`)) return;
    void run(key.access_key, () => consoleApi.setAccessKeyEnabled(project.id, key.access_key, !key.enabled), key.enabled ? 'Key disabled' : 'Key enabled');
  }

  return (
    <>
      <PageHeader title="Access keys" description="Apps use these to connect to this project's buckets. A key can reach this project and nothing else." onRefresh={() => void reload()} loading={loading}>
        {project.enabled && (
          <button onClick={() => setCreating(true)} className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium rounded-lg transition-colors whitespace-nowrap">
            <Plus className="w-4 h-4" /> Create key
          </button>
        )}
      </PageHeader>

      <PageState error={error} loading={loading} hasData={data !== null}>
        {data && data.length === 0 && (
          <EmptyState icon={<KeyRound className="w-16 h-16" />} title="No access keys yet" description="Create a key to connect an app to this project." />
        )}
        {data && data.length > 0 && (
          <Table headers={['Name', 'Access key', 'Status', 'Created', 'Last used', '']}>
            {data.map((key) => (
              <tr key={key.access_key}>
                <td className="px-4 py-3 text-gray-900 break-words max-w-[14rem]">{key.name}</td>
                <td className="px-4 py-3"><code className="text-xs text-gray-600">{key.access_key}</code></td>
                <td className="px-4 py-3"><Badge tone={key.enabled ? 'green' : 'gray'}>{key.enabled ? 'Enabled' : 'Disabled'}</Badge></td>
                <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{formatDate(key.created_at)}</td>
                <td className="px-4 py-3 text-gray-500 whitespace-nowrap">{key.last_used_at ? formatDate(key.last_used_at) : 'Never'}</td>
                <td className="px-4 py-3">
                  <div className="flex justify-end gap-2">
                    <button className={secondaryButton} disabled={busy !== null || !project.enabled} onClick={() => toggle(key)}>{key.enabled ? 'Disable' : 'Enable'}</button>
                    <button className={dangerButton} disabled={busy !== null || !project.enabled} onClick={() => remove(key)}>Delete</button>
                  </div>
                </td>
              </tr>
            ))}
          </Table>
        )}

        <div className="mt-8 bg-white border border-gray-200 rounded-xl p-5">
          <h3 className="text-sm font-medium text-gray-900 mb-1">Connecting an app</h3>
          <p className="text-sm text-gray-500 mb-4">
            Any S3-compatible tool works. Point it at <code className="text-xs">{API_ENDPOINT}</code>, in region <code className="text-xs">{API_REGION}</code>, with path-style addressing.
          </p>
          <ConnectionDetails accessKey="YOUR_ACCESS_KEY" secretKey="YOUR_SECRET_KEY" />
        </div>
      </PageState>

      {creating && (
        <CreateKeyModal
          projectId={project.id}
          onClose={() => setCreating(false)}
          onCreated={(key) => {
            setCreating(false);
            setCreated(key);
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
          <div className="space-y-4 max-h-[70vh] overflow-y-auto">
            <p className="text-sm text-amber-800 bg-amber-50 border border-amber-200 rounded-lg px-3 py-2">
              This is the only time the secret key is shown. If you lose it, delete this key and create another.
            </p>
            <CopyField label="Access key" value={created.access_key} />
            <CopyField label="Secret key" value={created.secret_key} />
            <ConnectionDetails accessKey={created.access_key} secretKey={created.secret_key} />
          </div>
        </Modal>
      )}
    </>
  );
}
