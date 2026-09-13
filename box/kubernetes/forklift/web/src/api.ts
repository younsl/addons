// The hand-written client, superseded by the code generated from
// src/openapi/openapi.yaml. Auth is cookie-based (set by POST
// /api/v1/login), so requests just need credentials: "include".
//
// New code should reach for the generated client instead:
//
//   services  @/services/v1/<domain>/api        one function per operation
//   types     @/services/v1/openapi-types       every component schema
//   queries   @/query/v1/openapi-query-options  queryOptions per GET
//   keys      @/query/v1/openapi-query-keys     for invalidation
//
// Mutations have no generated options: wrap the service function in
// useMutation yourself, because which queries to invalidate is not something
// the document can tell us.
//
// Three things here are not superseded and are staying until they find a
// better home:
//
//   - uploadArtifact and the upload/publication lifecycle helpers, which use
//     XMLHttpRequest for progress reporting and carry CSRF headers by hand
//   - UploadAPIError, the typed problem+json error the upload surface throws
//
// The types declared below duplicate generated ones. They move over as each is
// checked against what the generator produces; until then they are the ones in
// use, so they are not marked deprecated wholesale.

import { httpClient, type HttpMethod } from "@/lib/http/client/http-client";
import { API_PREFIX } from "@/services/paths";

// Types that came from the document rather than from here. The rest of this
// file still declares its own; they move over as each one is checked against
// what the generator produces.
import type {
  ApprovalAlarmPayload,
  GroupMapping,
  NotificationSamplePreview,
  NotificationSampleReceiver,
  PendingApprovalRepoList,
  UploadSession,
} from "@/services/v1/openapi-types";

export type {
  ApprovalAlarmPayload,
  GroupMapping,
  NotificationSamplePreview,
  NotificationSampleReceiver,
  PendingApprovalRepoList,
  UploadSession,
};

export interface RepoConfig {
  cache: {
    enabled: boolean;
    artifact_ttl: string;
    metadata_ttl: string;
    negative_ttl: string;
    max_size_bytes: number;
    eviction: string;
  };
  age_policy: {
    enabled: boolean;
    min_age: string;
    max_age: string;
    action: string;
  };
  approval: {
    enabled: boolean;
    mode?: string;
    auto_approve?: string[];
    auto_approve_clean?: boolean;
  };
  retention?: {
    idle_ttl?: string;
  };
  vuln?: {
    enabled: boolean;
    threshold?: string;
    action?: string;
    ignore?: string[];
    block_unscanned?: boolean;
  };
  license?: {
    enabled: boolean;
    action?: string;
    deny?: string[];
    allow?: string[];
    block_unresolved?: boolean;
  };
  policy_pipeline?: {
    schema_version: number;
    order: PolicyName[];
  };
  group: {
    members?: string[];
  };
  ip_acl?: {
    enabled: boolean;
    allow?: string[];
  };
  notify?: {
    receivers?: string[];
  };
  // public: anonymous downloads allowed on this repository (writes still authenticate).
  public?: boolean;
  upload?: {
    pypi_allow_legacy_zip?: boolean;
  };
  // Proxy upstream credentials. Secrets (password/token/value) are masked as
  // "********" in responses; sending the mask back keeps the stored secret.
  upstream_auth?: UpstreamAuthConfig;
}

export interface UpstreamAuthConfig {
  type?: "" | "basic" | "bearer" | "header";
  username?: string;
  password?: string;
  token?: string;
  header?: string;
  value?: string;
}

export type PolicyName = "vulnerability" | "license" | "age";

export interface Receiver {
  id: number;
  name: string;
  description: string;
  webhook_configured: boolean;
  enabled: boolean;
  created_by: string;
  created_at: string;
  updated_at: string;
  // Repositories whose notify config selects this receiver; deletion is
  // refused while non-empty.
  repositories: string[];
}

// RepoPermission is a role permission that grants access to a repository (its
// pattern matched the repo name), with the granting role and its user count.
export interface RepoPermission {
  role_id: number;
  role: string;
  repo_pattern: string;
  actions: string[];
  user_count: number;
}

// RepoToken is a personal access token that can reach a repository: a scoped
// token whose pattern matched, or an unscoped token inheriting the owner roles.
export interface RepoToken {
  token_id: number;
  name: string;
  owner: string;
  repo_pattern: string;
  actions: string[];
  unscoped: boolean;
  expires_at: string | null;
  last_used_at: string | null;
}

export interface Repository {
  id: number;
  name: string;
  format: string;
  type: string;
  upstream_url: string;
  config: RepoConfig;
  // Optional operator-facing free text.
  description?: string;
  disabled: boolean;
  // A predefined seed repository; protected from deletion (delete returns 403).
  seeded?: boolean;
  // Artifact aggregates, present in list responses only.
  artifact_count?: number;
  total_size?: number;
  // Packages awaiting approval in this repository (list responses only).
  pending_approval_count?: number;
  capabilities?: {
    read: boolean;
    write: boolean;
    delete: boolean;
    upload: boolean;
  };
  publish_methods?: ("mvn" | "npm" | "twine")[];
  // Vulnerability-scan aggregates (list responses only): how many stored
  // artifacts are scanned, and how many of those are clean (no advisories).
  scanned_count?: number;
  clean_count?: number;
  // Effective write capability for the current principal (detail response).
  can_write?: boolean;
}

// OCITagInfo mirrors the Harbor-style image view row for OCI repositories.
export interface OCITagInfo {
  name: string;
  tag: string;
  digest: string;
  media_type: string;
  kind: "chart" | "image" | "artifact" | "index";
  size: number;
  platforms: string[];
  pushed_at: string;
  pushed_by: string;
  pulled_at: string | null;
  pulled_by: string;
  // An OCI manifest is an ordinary artifact row, so it carries labels like every
  // other format; path is the stored manifest path the label endpoints take.
  path: string;
  labels: ArtifactLabel[];
  can_label: boolean;
}

// OCIArtifactDetail is the Harbor-style drill-down for one OCI artifact.
export interface OCIArtifactDetail {
  info: OCITagInfo;
  manifest_json: unknown;
  config_json?: unknown;
  image?: {
    os: string;
    architecture: string;
    created: string;
    entrypoint: string[] | null;
    cmd: string[] | null;
    env: string[] | null;
    layers: number;
  };
  chart?: { values_yaml: string; readme_md: string };
  children?: { digest: string; platform: string; size: number }[];
  path: string;
  labels: ArtifactLabel[];
  can_label: boolean;
}

export interface UploadPlan {
  path: string;
  package: string;
  version: string;
  content_type: string;
  size: number;
  approval_required: boolean;
  exists: boolean;
}

export interface UploadResult extends UploadPlan {
  approval_status: "not_required" | "pending" | "approved" | "rejected" | "audit";
}

// RepositoryName is the slim repository shape returned by /repository-names for
// token-scope autocomplete (available to any authenticated user).
export interface RepositoryName {
  name: string;
  format: string;
  type: string;
}

export interface Me {
  authenticated: boolean;
  username?: string;
  source?: string;
  admin?: boolean;
  // approver: may decide package approvals (admin, or a role with the approve action).
  approver?: boolean;
  // auditor: read-only access to the admin surfaces and the package approval
  // surface (queue, request detail, version denies); may view but not decide
  // (admin, or a role with the audit action).
  auditor?: boolean;
  // security: may edit a repository's security policy on the Security tab
  // (admin, or a role with the security action). Distinct from approver: this
  // rewrites the policy, approving decides one package against it.
  security?: boolean;
  csrf_token?: string;
  // impersonator: the administrator acting as this user. Present only while an
  // impersonated session is active; every permission above is the impersonated
  // user's own, so the banner is the only thing that changes for the admin.
  impersonator?: string;
}

export interface Version {
  version: string;
  commit: string;
  oidc_enabled: boolean;
}

// Site-wide announcement (Markdown source); empty body means none is set.
export interface Announcement {
  body: string;
  updated_by?: string;
  updated_at?: string;
}

export interface HAStatus {
  enabled: boolean;
  mode: string;
  backend: string;
  storage_endpoint?: string;
  identity: string;
  leader: string;
  is_leader: boolean;
  role: string;
  lease_name?: string;
  fencing_token?: number;
  started_at?: string;
  version?: string;
}

// StorageStats is the object-storage overview for the admin Storage page.
export interface StorageStats {
  backend: string; // "fs" | "s3"
  mode: string; // "filesystem" | "minio" | "s3"
  endpoint?: string;
  bucket?: string;
  prefix?: string;
  blob_count: number;
  blob_bytes: number;
  minio?: MinIOStats;
  minio_error?: string;
  // fs: capacity of the volume the data directory lives on. Present only for the
  // filesystem backend; a bucket is not a disk that fills up, so S3 has none.
  fs?: DiskStats;
  fs_error?: string;
  // Artifacts whose bytes are absent from the blob store, so requests for them
  // fail. Empty when metadata and the blob store agree.
  dangling: DanglingRef[];
}

// DanglingRef is one artifact whose metadata points at blob bytes that are not in
// the blob store.
export interface DanglingRef {
  repository: string;
  repo_id: number;
  path: string;
  sha256: string;
  role?: string;
  first_seen: string;
  last_seen: string;
  hits: number;
  // Response codes these failures produced, most frequent first: a fetch answers
  // 500, a publish or delete answers 503.
  statuses: StatusCount[];
  // Code the most recent failure returned.
  last_status?: number;
}

export interface StatusCount {
  code: number;
  count: number;
}

// DiskStats is the filesystem capacity behind the data directory (fs backend).
// available_bytes excludes any root reserve, so used + available can fall short
// of total.
export interface DiskStats {
  path?: string;
  total_bytes: number;
  used_bytes: number;
  available_bytes: number;
  usage_ratio: number;
}

// MinIOStats is the live MinIO cluster metadata (present only for a MinIO backend).
export interface MinIOStats {
  total_capacity_bytes: number;
  used_bytes: number;
  available_bytes: number;
  usage_ratio: number;
  logical_used_bytes: number;
  object_count: number;
  bucket_count: number;
  online_drives: number;
  offline_drives: number;
  servers: number;
  version?: string;
}

export interface Token {
  id: number;
  name: string;
  description: string;
  scopes_json: string;
  expires_at: string | null;
  last_used_at: string | null;
  created_at: string;
}

export interface Artifact {
  path: string;
  version: string;
  size: number;
  content_type: string;
  published_at: string | null;
  cached_at: string;
  last_accessed_at: string;
  cached_by: string;
  max_severity?: string;
  vuln_ids?: string[];
  vuln_counts?: Record<string, number>;
  vuln_advisories?: { id: string; severity: string; score?: string }[];
  vuln_source?: string;
  vuln_scanned_at?: string | null;
  licenses?: string[];
  license_source?: string;
  license_resolved_at?: string | null;
  publication_id?: string;
  artifact_role?: "primary" | "metadata" | "checksum" | "index";
  // Operator tags on this artifact, and whether this viewer may change them
  // (administrator on the repository, or the principal who uploaded it). The
  // server answers the permission per artifact, so the UI never has to infer it.
  labels: ArtifactLabel[];
  can_label: boolean;
  // Set when the artifact's bytes are absent from the blob store, so requests for
  // it fail. blob_missing_since is when that was first observed.
  blob_missing?: boolean;
  blob_missing_since?: string | null;
  blob_missing_statuses?: StatusCount[];
  blob_missing_last_seen?: string | null;
  blob_missing_last_status?: number;
}

// ArtifactLabel is one operator tag on one artifact, with its provenance.
export interface ArtifactLabel {
  label: string;
  created_by: string;
  created_at: string;
}

// ArtifactLabelList is what the label endpoints answer with: the artifact's
// labels as they now stand, plus this caller's permission to change them.
export interface ArtifactLabelList {
  path: string;
  labels: ArtifactLabel[];
  can_label: boolean;
}

export interface ArtifactPublication {
  id: string;
  format: string;
  coordinate: string;
  package: string;
  version: string;
  asset_count: number;
  total_size: number;
  actions: ("replace" | "extend" | "delete" | "yank" | "unyank")[];
  yanked: boolean;
  // Assets of this publication whose blob bytes are missing, so they cannot be
  // served. Counted across the whole repository, not just the loaded page.
  broken_assets?: number;
  created_by: string;
  created_at: string;
  updated_at: string;
}

export interface ArtifactList {
  count: number;
  total_size: number;
  // filtered is the number of artifacts matching the active search across all
  // pages; artifacts carries only the requested page.
  filtered: number;
  artifacts: Artifact[];
  publications: ArtifactPublication[];
}

// ArtifactQuery narrows and pages the artifact listing server-side. q matches
// any rendered column as a substring, or as a regular expression when regex.
export interface ArtifactQuery {
  q?: string;
  regex?: boolean;
  limit?: number;
  offset?: number;
}

export interface UploadedArtifact {
  path: string;
  role: "primary" | "metadata" | "checksum" | "index";
  size: number;
  sha256: string;
}

export interface ArtifactUploadResult {
  upload_id: string;
  repository: string;
  format: string;
  coordinate: string;
  created: UploadedArtifact[];
  replaced: UploadedArtifact[];
  derived: UploadedArtifact[];
  scan_status: "queued" | "deferred" | "disabled";
  durability: "local" | "async";
  warnings: { code: string; detail: string }[];
}

export interface UploadProblem {
  type: string;
  title: string;
  status: number;
  code: string;
  detail: string;
  upload_id?: string;
  field_errors?: Record<string, string[]>;
  conflicts?: string[];
  conflict_action?: "replace" | "reject";
  retryable?: boolean;
}

export class UploadAPIError extends Error {
  constructor(public problem: UploadProblem) {
    super(problem.detail || problem.title);
    this.name = "UploadAPIError";
  }
}

export interface RoleRef {
  id: number;
  name: string;
}

export interface User {
  id: number;
  username: string;
  source: string;
  email: string;
  disabled: boolean;
  // robot: a token-only service account that cannot log in interactively.
  robot: boolean;
  created_at: string;
  last_login_at: string | null;
  roles: RoleRef[];
  lockout_enabled: boolean;
  locked: boolean;
  // failed_login_count: consecutive failed password logins (locks at 5).
  failed_login_count?: number;
  // protected: the default admin, which cannot be locked out.
  protected: boolean;
  // token_count: personal access tokens the user owns.
  token_count: number;
}

export interface Permission {
  id: number;
  repo_pattern: string;
  actions: string[];
}

export interface Role {
  id: number;
  name: string;
  description: string;
  created_at: string;
  permissions: Permission[];
  user_count: number;
  // managed: declared via the chart (declarative RBAC) vs created in the UI.
  managed: boolean;
}

export interface AuditLog {
  id: number;
  event: string;
  path: string;
  username: string;
  method: string;
  status: number;
  client_ip: string;
  user_agent: string;
  created_at: string;
}

export interface AuditLogList {
  count: number;
  logs: AuditLog[];
}

export interface Approval {
  id: number;
  repo_name: string;
  package: string;
  status: string;
  requested_by: string;
  decided_by: string;
  note: string;
  request_count: number;
  last_requested_version: string;
  first_requested_at: string;
  last_requested_at: string;
  decided_at: string | null;
  vuln_severity?: string;
  vuln_ids?: string[];
  vuln_scope?: string;
  vuln_counts?: Record<string, number>;
  vuln_advisories?: { id: string; severity: string; score?: string }[];
  vuln_source?: string;
  vuln_scanned_at?: string;
  vuln_scan_ms?: number;
  reviewers?: string[];
  upstream_url?: string;
  upstream_package_url?: string;
  notified_receivers?: string[];
  // Notification delivery outcome, recorded when the alarm was actually sent.
  notified_at?: string;
  notify_result?: string; // "delivered" | "failed"
  notify_duration_ms?: number;
  notify_detail?: string; // concrete outcome, e.g. "HTTP 200" / "no response from webhook"
}

export interface ApprovalList {
  count: number;
  approvals: Approval[];
}

// PendingRepo summarises one repository's approval queue: total pending and how
// many of those are Clean (approved by a Clean-only bulk approve).
export interface PendingRepo {
  id: number;
  repo_name: string;
  format: string;
  type: string;
  pending: number;
  clean: number;
}

export interface VersionDeny {
  id: number;
  repo_name: string;
  package: string;
  version: string;
  reason: string;
  created_by: string;
  created_at: string;
}

export interface VersionDenyList {
  count: number;
  denies: VersionDeny[];
}

// SearchResult is the grouped global-search response. A null section means the
// caller lacks access to that surface; an empty array means access but no match.
export interface SearchResult {
  query: string;
  repositories: { id: number; name: string; format: string; type: string }[] | null;
  artifacts: { repo_id: number; repo_name: string; path: string; size: number }[] | null;
  labels: { repo_id: number; repo_name: string; path: string; label: string }[] | null;
  approvals: { id: number; repo_name: string; package: string; status: string }[] | null;
  users: { id: number; username: string; robot: boolean }[] | null;
  roles: { id: number; name: string; description: string }[] | null;
  // Exact total matches per permitted section; item lists are capped by limit.
  counts: Record<string, number>;
}

export interface UpstreamHealth {
  applicable: boolean;
  reachable?: boolean;
  status?: number;
  latency_ms?: number;
  error?: string;
}

// req is the hand-written client's spelling of a request: a path relative to the
// API prefix, and a method as a bare string. The transport itself now lives in
// httpClient, which the generated services call directly - the two clients would
// otherwise each carry their own deadline and error handling.
function req<T>(
  method: string,
  path: string,
  body?: unknown,
  opts?: { signal?: AbortSignal; timeoutMs?: number },
): Promise<T> {
  return httpClient.request<T>(
    method.toUpperCase() as HttpMethod,
    `${API_PREFIX}${path}`,
    body,
    opts,
  );
}

export function uploadArtifact(
  repositoryId: number,
  form: FormData,
  options: {
    idempotencyKey: string;
    csrfToken?: string;
    signal?: AbortSignal;
    onProgress?: (loaded: number, total: number) => void;
  },
): Promise<ArtifactUploadResult> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", `/api/v1/repositories/${repositoryId}/uploads`);
    xhr.withCredentials = true;
    xhr.timeout = 30 * 60 * 1000;
    xhr.setRequestHeader("Idempotency-Key", options.idempotencyKey);
    if (options.csrfToken) xhr.setRequestHeader("X-CSRF-Token", options.csrfToken);

    const abort = () => xhr.abort();
    if (options.signal?.aborted) {
      reject(options.signal.reason ?? new DOMException("upload aborted", "AbortError"));
      return;
    }
    options.signal?.addEventListener("abort", abort, { once: true });
    xhr.upload.onprogress = (event) => options.onProgress?.(event.loaded, event.lengthComputable ? event.total : 0);
    xhr.onerror = () => reject(new Error("The upload connection failed."));
    xhr.ontimeout = () => reject(new DOMException("upload timed out", "TimeoutError"));
    xhr.onabort = () => reject(new DOMException("upload aborted", "AbortError"));
    xhr.onload = () => {
      let data: unknown;
      try {
        data = xhr.responseText ? JSON.parse(xhr.responseText) : undefined;
      } catch {
        data = undefined;
      }
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(data as ArtifactUploadResult);
        return;
      }
      const problem = data as UploadProblem | undefined;
      if (problem?.code) {
        reject(new UploadAPIError(problem));
      } else {
        reject(new Error(xhr.responseText.trim() || xhr.statusText || "Upload failed"));
      }
    };
    xhr.onloadend = () => options.signal?.removeEventListener("abort", abort);
    xhr.send(form);
  });
}

async function uploadLifecycleRequest<T>(method: "POST" | "DELETE", repositoryId: number, uploadId: string, suffix: string, csrfToken?: string): Promise<T> {
  const response = await fetch(`/api/v1/repositories/${repositoryId}/uploads/${encodeURIComponent(uploadId)}${suffix}`, {
    method,
    credentials: "include",
    headers: csrfToken ? { "X-CSRF-Token": csrfToken } : undefined,
  });
  const text = await response.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!response.ok) {
    const problem = data as UploadProblem | undefined;
    if (problem?.code) throw new UploadAPIError(problem);
    throw new Error(text.trim() || response.statusText);
  }
  return data as T;
}

export const commitArtifactUpload = (repositoryId: number, uploadId: string, csrfToken?: string) =>
  uploadLifecycleRequest<ArtifactUploadResult>("POST", repositoryId, uploadId, "/commit", csrfToken);

export const cancelArtifactUpload = (repositoryId: number, uploadId: string, csrfToken?: string) =>
  uploadLifecycleRequest<void>("DELETE", repositoryId, uploadId, "", csrfToken);

export interface PublicationLifecycleResult {
  publication_id: string;
  coordinate: string;
  deleted: string[];
  derived_updated: UploadedArtifact[];
  derived_deleted: string[];
  yanked?: boolean;
}

async function publicationLifecycleRequest(repositoryId: number, publicationId: string, method: "POST" | "DELETE", suffix: string, csrfToken?: string, body?: unknown) {
  const response = await fetch(`/api/v1/repositories/${repositoryId}/publications/${encodeURIComponent(publicationId)}${suffix}`, {
    method, credentials: "include",
    headers: { ...(csrfToken ? { "X-CSRF-Token": csrfToken } : {}), ...(body ? { "Content-Type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!response.ok) {
    const problem = data as UploadProblem | undefined;
    if (problem?.code) throw new UploadAPIError(problem);
    throw new Error(text.trim() || response.statusText);
  }
  return data as PublicationLifecycleResult;
}

export const deleteArtifactPublication = (repositoryId: number, publicationId: string, csrfToken?: string) =>
  publicationLifecycleRequest(repositoryId, publicationId, "DELETE", "", csrfToken);

export const setCargoPublicationYanked = (repositoryId: number, publicationId: string, yanked: boolean, csrfToken?: string) =>
  publicationLifecycleRequest(repositoryId, publicationId, "POST", "/yank", csrfToken, { yanked });

/**
 * The hand-written operation list.
 *
 * @deprecated Use the generated services instead — `@/services/v1/<domain>/api`
 * has a function per operation, typed from the document rather than from here.
 * For reads, `openApiQueryOptions` already wraps them with a query key.
 *
 * Every method below has a generated counterpart. The names differ: they are
 * derived from the route, so `api.listRepositories()` is `listRepositories()`
 * from `@/services/v1/repositories/api`, and `api.updateRepositorySecurity(id, config)`
 * is `updateRepositorySecurity({ path: { id }, body: { config } })`.
 *
 * This stays until the screens have moved; it is not scheduled for removal
 * while 24 files still import it.
 */
export const api = {
  me: () => req<Me>("GET", "/me"),
  landingStats: () => req<{ repositories: number; artifacts: number }>("GET", "/stats/landing"),
  listOCITags: (repoId: number) => req<{ tags: OCITagInfo[] }>("GET", `/repositories/${repoId}/oci-tags`),
  getOCIDetail: (repoId: number, name: string, ref: string) =>
    req<OCIArtifactDetail>("GET", `/repositories/${repoId}/oci-detail?name=${encodeURIComponent(name)}&ref=${encodeURIComponent(ref)}`),
  login: (username: string, password: string) =>
    req<{ username: string }>("POST", "/login", { username, password }),
  logout: () => req<void>("POST", "/logout"),
  version: () => req<Version>("GET", "/version"),
  getAnnouncement: () => req<Announcement>("GET", "/announcement"),
  putAnnouncement: (body: string) => req<Announcement>("PUT", "/announcement", { body }),
  getHA: () => req<HAStatus>("GET", "/ha"),
  getStorage: () => req<StorageStats>("GET", "/storage"),
  stepDownHA: () => req<{ status: string }>("POST", "/ha/step-down"),

  listReceivers: () => req<Receiver[]>("GET", "/notification/receivers"),
  createReceiver: (body: { name: string; description: string; webhook_url: string; enabled: boolean }) =>
    req<Receiver>("POST", "/notification/receivers", body),
  updateReceiver: (id: number, body: { name: string; description: string; webhook_url: string; enabled: boolean }) =>
    req<Receiver>("PUT", `/notification/receivers/${id}`, body),
  deleteReceiver: (id: number) => req<void>("DELETE", `/notification/receivers/${id}`),
  testReceiver: (id: number) => req<{ status: string }>("POST", `/notification/receivers/${id}/test`),
  testWebhookURL: (webhook_url: string, name: string) =>
    req<{ status: string }>("POST", "/notification/test", { webhook_url, name }),
  previewRepoSample: (id: number) =>
    req<NotificationSamplePreview>("GET", `/repositories/${id}/notification/sample`),
  sendRepoSample: (id: number) =>
    req<{ results: { name: string; ok: boolean; error?: string }[] }>(
      "POST", `/repositories/${id}/notification/sample`),

  search: (q: string, limit = 5, signal?: AbortSignal) =>
    req<SearchResult>("GET", `/search?q=${encodeURIComponent(q)}&limit=${limit}`, undefined, { signal }),

  listRepositories: () => req<Repository[]>("GET", "/repositories"),
  listRepositoryNames: () => req<RepositoryName[]>("GET", "/repository-names"),
  getRepository: (id: number) => req<Repository>("GET", `/repositories/${id}`),
  createRepository: (body: unknown) => req<Repository>("POST", "/repositories", body),
  updateRepository: (id: number, body: unknown) =>
    req<Repository>("PUT", `/repositories/${id}`, body),
  // Security tab: the non-admin write path. Only the policy sections of the
  // config are read server-side, so the upstream URL and its credentials are
  // not sent and cannot be changed here.
  updateRepositorySecurity: (id: number, config: unknown) =>
    req<Repository>("PUT", `/repositories/${id}/security`, { config }),
  deleteRepository: (id: number) => req<void>("DELETE", `/repositories/${id}`),
  repositoryPermissions: (id: number) =>
    req<RepoPermission[]>("GET", `/repositories/${id}/permissions`),
  repositoryTokens: (id: number) =>
    req<RepoToken[]>("GET", `/repositories/${id}/tokens`),
  setRepositoryDisabled: (id: number, disabled: boolean) =>
    req<Repository>("POST", `/repositories/${id}/disabled`, { disabled }),

  listArtifacts: (id: number, query: ArtifactQuery = {}) =>
    req<ArtifactList>("GET", `/repositories/${id}/artifacts?q=${encodeURIComponent(query.q ?? "")}` +
      `&regex=${query.regex ? "true" : "false"}&limit=${query.limit ?? 50}&offset=${query.offset ?? 0}`),
  validateArtifactUpload: (id: number, body: { path: string; size: number; content_type: string }) =>
    req<UploadPlan>("POST", `/repositories/${id}/artifacts/validate-upload`, body),
  uploadArtifact: (id: number, path: string, file: File, onProgress?: (percent: number) => void) =>
    new Promise<UploadResult>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open("PUT", `/api/v1/repositories/${id}/artifacts/upload?path=${encodeURIComponent(path)}`);
      xhr.withCredentials = true;
      xhr.setRequestHeader("Content-Type", file.type || "application/octet-stream");
      xhr.upload.onprogress = (event) => {
        if (event.lengthComputable) onProgress?.(Math.round((event.loaded / event.total) * 100));
      };
      xhr.onerror = () => reject(new Error("Upload failed: network error"));
      xhr.onabort = () => reject(new Error("Upload cancelled"));
      xhr.onload = () => {
        let data: unknown;
        try { data = xhr.responseText ? JSON.parse(xhr.responseText) : undefined; } catch { data = undefined; }
        if (xhr.status < 200 || xhr.status >= 300) {
          reject(new Error((data as { error?: string } | undefined)?.error || xhr.responseText.trim() || xhr.statusText));
          return;
        }
        resolve(data as UploadResult);
      };
      xhr.send(file);
    }),
  // force removes an artifact whose bytes are missing from the blob store, which
  // the ordinary delete refuses for managed publications. The server only honours
  // it once it has confirmed the bytes are gone.
  deleteArtifact: (id: number, path: string, force = false) =>
    req<void>("DELETE", `/repositories/${id}/artifacts?path=${encodeURIComponent(path)}${force ? "&force=true" : ""}`),
  listArtifactLabels: (id: number, path: string) =>
    req<ArtifactLabelList>("GET", `/repositories/${id}/artifacts/labels?path=${encodeURIComponent(path)}`),
  addArtifactLabel: (id: number, path: string, label: string) =>
    req<ArtifactLabelList>("POST", `/repositories/${id}/artifacts/labels`, { path, label }),
  removeArtifactLabel: (id: number, path: string, label: string) =>
    req<ArtifactLabelList>("DELETE",
      `/repositories/${id}/artifacts/labels?path=${encodeURIComponent(path)}&label=${encodeURIComponent(label)}`),
  purgeArtifacts: (id: number) =>
    req<{ deleted: number }>("DELETE", `/repositories/${id}/artifacts`),
  upstreamHealth: (id: number, signal?: AbortSignal) =>
    req<UpstreamHealth>("GET", `/repositories/${id}/upstream-health`, undefined, { signal }),
  checkUpstream: (url: string, signal?: AbortSignal, auth?: UpstreamAuthConfig) =>
    req<UpstreamHealth>("POST", "/repositories/check-upstream",
      auth?.type ? { url, auth } : { url }, { signal }),
  // Artifacts in this repository whose blob bytes are missing, so views that show
  // artifact paths can mark the affected rows.
  listDangling: (id: number) => req<DanglingRef[]>("GET", `/repositories/${id}/dangling`),
  listAuditLogs: (id: number, event = "", limit = 100, offset = 0) =>
    req<AuditLogList>(
      "GET",
      `/repositories/${id}/audit-logs?event=${encodeURIComponent(event)}&limit=${limit}&offset=${offset}`,
    ),

  listUsers: () => req<User[]>("GET", "/users"),
  createUser: (body: { username: string; password?: string; email?: string; role_ids?: number[]; robot?: boolean }) =>
    req<{ id: number; username: string }>("POST", "/users", body),
  updateUser: (id: number, body: { password?: string; disabled?: boolean; lockout_enabled?: boolean; unlock?: boolean }) =>
    req<User>("PUT", `/users/${id}`, body),
  deleteUser: (id: number) => req<void>("DELETE", `/users/${id}`),
  // Swaps the caller's session cookie for one acting as the target user. Admin
  // only, and only from a browser session: the reason is recorded server-side.
  impersonateUser: (id: number, reason: string) =>
    req<{ username: string; source: string; impersonator: string; expires_in: string }>(
      "POST", `/users/${id}/impersonate`, { reason }),
  stopImpersonation: () =>
    req<{ username: string; source: string }>("POST", "/impersonate/stop"),
  assignRole: (userId: number, roleId: number) =>
    req<void>("POST", `/users/${userId}/roles`, { role_id: roleId }),
  removeRole: (userId: number, roleId: number) =>
    req<void>("DELETE", `/users/${userId}/roles/${roleId}`),

  listRoles: () => req<Role[]>("GET", "/roles"),
  createRole: (body: { name: string; description?: string; permissions?: { repo_pattern: string; actions: string[] }[] }) =>
    req<Role>("POST", "/roles", body),
  deleteRole: (id: number) => req<void>("DELETE", `/roles/${id}`),
  addPermission: (roleId: number, body: { repo_pattern: string; actions: string[] }) =>
    req<Permission>("POST", `/roles/${roleId}/permissions`, body),
  deletePermission: (roleId: number, permId: number) =>
    req<void>("DELETE", `/roles/${roleId}/permissions/${permId}`),

  listApprovals: (repo = "", status = "", limit = 100, offset = 0, q = "", regex = false) =>
    req<ApprovalList>(
      "GET",
      `/approvals?repo=${encodeURIComponent(repo)}&status=${encodeURIComponent(status)}&limit=${limit}&offset=${offset}` +
        `&q=${encodeURIComponent(q)}&regex=${regex ? "true" : "false"}`,
    ),
  approvalCount: (status = "pending", repo = "") =>
    req<{ count: number }>(
      "GET",
      `/approvals/count?status=${encodeURIComponent(status)}&repo=${encodeURIComponent(repo)}`,
    ),
  // Per-repository approval-queue overview, only for repos with a live queue:
  // total pending and how many of those are Clean.
  pendingApprovalRepos: () =>
    req<{ repos: PendingRepo[] }>("GET", "/approvals/pending-repos"),
  getApproval: (id: number) => req<Approval>("GET", `/approvals/${id}`),
  createApproval: (body: { repo: string; package: string; status: string; note?: string }) =>
    req<Approval>("POST", "/approvals", body),
  approveAllPending: (repo: string, note = "", cleanOnly = false) =>
    req<{ approved: number }>("POST", "/approvals/approve-all", { repo, note, clean_only: cleanOnly }),
  approveApproval: (id: number, note = "") =>
    req<Approval>("POST", `/approvals/${id}/approve`, { note }),
  rejectApproval: (id: number, note = "") =>
    req<Approval>("POST", `/approvals/${id}/reject`, { note }),

  listVersionDenies: (repo = "", limit = 100, offset = 0) =>
    req<VersionDenyList>(
      "GET",
      `/version-denies?repo=${encodeURIComponent(repo)}&limit=${limit}&offset=${offset}`,
    ),
  createVersionDeny: (body: { repo: string; package: string; version: string; reason?: string }) =>
    req<VersionDeny>("POST", "/version-denies", body),
  deleteVersionDeny: (id: number) => req<void>("DELETE", `/version-denies/${id}`),

  listTokens: () => req<Token[]>("GET", "/tokens"),
  createToken: (body: unknown) => req<{ token: string; name: string }>("POST", "/tokens", body),
  updateToken: (id: number, scopes: unknown[]) => req<void>("PATCH", `/tokens/${id}`, { scopes }),
  deleteToken: (id: number) => req<void>("DELETE", `/tokens/${id}`),

  // Admin/auditor: a target user's tokens, managed from the user detail page.
  // Listing is read-only for auditors; create and revoke are admin-only.
  listUserTokens: (userId: number) => req<Token[]>("GET", `/users/${userId}/tokens`),
  createUserToken: (userId: number, body: unknown) =>
    req<{ token: string; name: string }>("POST", `/users/${userId}/tokens`, body),
  updateUserToken: (userId: number, tokenId: number, scopes: unknown[]) =>
    req<void>("PATCH", `/users/${userId}/tokens/${tokenId}`, { scopes }),
  deleteUserToken: (userId: number, tokenId: number) =>
    req<void>("DELETE", `/users/${userId}/tokens/${tokenId}`),
};
