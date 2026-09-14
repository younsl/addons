import { ScopablePlugin } from './types';

/**
 * Plugins a token may be scoped to when `pat.scopablePlugins` is absent from
 * app-config. Every backend plugin registered in `packages/backend/src/index.ts`
 * that serves an API under `/api/<id>` is listed, except `pat` itself and the
 * framework plugins with no API worth delegating (`app`, `auth`).
 *
 * Keep this list in step with the backend when a plugin is added or removed.
 * Setting `pat.scopablePlugins` in app-config replaces the list entirely.
 */
export const DEFAULT_SCOPABLE_PLUGINS: readonly ScopablePlugin[] = [
  { id: 'catalog', label: 'Catalog', description: 'Entities, locations and refresh' },
  { id: 'search', label: 'Search', description: 'Search queries' },
  { id: 'techdocs', label: 'TechDocs', description: 'Documentation sites and metadata' },
  { id: 'scaffolder', label: 'Scaffolder', description: 'Templates and task runs' },
  { id: 'permission', label: 'Permission', description: 'Authorization decisions' },
  { id: 'proxy', label: 'Proxy', description: 'Endpoints under proxy.endpoints' },
  { id: 'gitlab', label: 'GitLab', description: 'GitLab API passthrough' },
  { id: 'sonarqube', label: 'SonarQube', description: 'Project quality metrics' },
  { id: 'platforms', label: 'Platforms', description: 'Platform links and visitor stats' },
  { id: 'openapi-registry', label: 'API Registry', description: 'OpenAPI specifications' },
  { id: 'argocd-appset', label: 'ArgoCD AppSets', description: 'ApplicationSet inventory' },
  { id: 'catalog-health', label: 'Catalog Health', description: 'catalog-info.yaml coverage' },
  {
    id: 'iam-user-audit',
    label: 'IAM User Audit',
    description: 'AWS IAM user audit and password resets',
  },
  { id: 's3-log-extract', label: 'S3 Log Extract', description: 'Log extraction jobs from S3' },
  { id: 'opencost', label: 'OpenCost', description: 'Kubernetes cost reports' },
  {
    id: 'opensearch-account',
    label: 'OpenSearch Account',
    description: 'OpenSearch user accounts',
  },
  {
    id: 'opensearch-viewer',
    label: 'OpenSearch Viewer',
    description: 'Index and document browsing',
  },
  {
    id: 'opensearch-scaling',
    label: 'OpenSearch Scaling',
    description: 'Cluster scaling operations',
  },
  { id: 'gitlab-token-audit', label: 'GitLab Token Audit', description: 'Access token inventory' },
  { id: 'slack-mr-bot', label: 'Slack MR Bot', description: 'Merge request notifications' },
];
