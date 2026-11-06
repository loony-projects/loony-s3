import { useParams } from 'react-router-dom';

/** Where the bucket pages live: under the open project, or at the top level for the root
 * key, so the same pages serve both. */
export function useBucketRoutes() {
  const { projectId } = useParams();
  const list = projectId ? `/projects/${projectId}/buckets` : '/';
  return {
    list,
    bucket: (name: string) => (projectId ? `/projects/${projectId}/buckets/${name}` : `/buckets/${name}`),
  };
}
