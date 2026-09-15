export type ScopeAccess = 'read' | 'write';

export interface TokenScope {
  plugin: string;
  access: ScopeAccess;
}

export type TokenState = 'active' | 'expired' | 'revoked';

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
  description: string;
  expiresInDays: number;
  scopes: TokenScope[];
}

export interface UpdateTokenInput {
  description?: string;
  scopes?: TokenScope[];
}

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
  details: string | null;
  createdAt: string;
}

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
  auditRetentionDays: number;
  scopablePlugins: ScopablePlugin[];
}
