/** Access level a scope grants on one plugin. `read` covers GET/HEAD/OPTIONS, `write` covers every method. */
export type ScopeAccess = 'read' | 'write';

/** One fine-grained grant: a target plugin id plus the access level allowed on it. */
export interface TokenScope {
  plugin: string;
  access: ScopeAccess;
}

export type TokenState = 'active' | 'expired' | 'revoked';

/** Token metadata returned to the admin UI. Never contains the secret or its hash. */
export interface PatToken {
  id: string;
  name: string;
  description: string | null;
  tokenPrefix: string;
  scopes: TokenScope[];
  state: TokenState;
  createdBy: string;
  createdAt: string;
  expiresAt: string;
  revokedAt: string | null;
  revokedBy: string | null;
  lastUsedAt: string | null;
  lastUsedIp: string | null;
  useCount: number;
}

export interface CreateTokenInput {
  name: string;
  description?: string;
  expiresInDays: number;
  scopes: TokenScope[];
}

export interface UpdateTokenInput {
  name?: string;
  description?: string;
  scopes?: TokenScope[];
}

/** Full-secret result of a create call. The `token` value is shown once and never stored. */
export interface CreatedToken {
  token: string;
  record: PatToken;
}

export type AuditEventType =
  | 'token.created'
  | 'token.updated'
  | 'token.revoked'
  | 'token.deleted'
  | 'api.request'
  | 'api.denied';

export type AuditOutcome = 'allowed' | 'denied';

export type DenyReason =
  | 'invalid_token'
  | 'expired'
  | 'revoked'
  | 'scope_missing'
  | 'scope_insufficient'
  | 'plugin_forbidden'
  | 'gateway_unavailable'
  | 'throttled';

export interface AuditEvent {
  id: number;
  eventType: AuditEventType;
  outcome: AuditOutcome;
  reason: string | null;
  tokenId: string | null;
  tokenName: string | null;
  actor: string | null;
  pluginId: string | null;
  method: string | null;
  path: string | null;
  statusCode: number | null;
  durationMs: number | null;
  ip: string | null;
  userAgent: string | null;
  /** JSON payload for lifecycle events, such as the before/after scopes of an update. */
  details: string | null;
  createdAt: string;
}

export type NewAuditEvent = Omit<AuditEvent, 'id' | 'createdAt'>;

export interface AuditQuery {
  limit: number;
  offset: number;
  tokenId?: string;
  eventType?: AuditEventType;
  outcome?: AuditOutcome;
  search?: string;
}

export interface AuditPage {
  items: AuditEvent[];
  total: number;
}

export interface AuditSummary {
  windowHours: number;
  requests: number;
  denied: number;
  activeTokens: number;
  expiringSoonTokens: number;
}

export interface ScopablePlugin {
  id: string;
  label: string;
  description: string | null;
}

export interface PatSettings {
  maxExpiryDays: number;
  /** Days audit events are kept before the daily purge removes them. */
  auditRetentionDays: number;
  scopablePlugins: ScopablePlugin[];
}
