import { LoggerService } from '@backstage/backend-plugin-api';
import { Config } from '@backstage/config';
import { AuditStore } from './AuditStore';
import { DEFAULT_SCOPABLE_PLUGINS } from './defaults';
import { checkScope, normalizeScopes, pluginIdFromPath, SELF_PLUGIN_ID } from './scopes';
import {
  addDays,
  generateToken,
  HARD_MAX_EXPIRY_DAYS,
  hashToken,
  resolveExpiryDays,
  tokenPrefix,
} from './token';
import { rowToToken, TokenStore, tokenState } from './TokenStore';
import {
  AuditPage,
  AuditQuery,
  AuditSummary,
  CreatedToken,
  DenyReason,
  PatSettings,
  PatToken,
  ScopablePlugin,
  TokenScope,
  UpdateTokenInput,
} from './types';

const NAME_MAX = 100;
const DESCRIPTION_MAX = 500;
export const DEFAULT_AUDIT_RETENTION_DAYS = 365;
/** Token names are identifiers: they appear in logs, headers and URLs, so only a safe subset is allowed. */
export const NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

export class ValidationError extends Error {
  readonly statusCode = 400;
}

export class NotFoundError extends Error {
  readonly statusCode = 404;
}

export class ConflictError extends Error {
  readonly statusCode = 409;
}

/** Result of authenticating a bearer PAT against a target plugin and method. */
export type AuthenticateResult =
  | { ok: true; token: PatToken; scope: TokenScope }
  | { ok: false; reason: DenyReason; token?: PatToken };

export function readSettings(config: Config): PatSettings {
  const configured = config.getOptionalNumber('pat.maxExpiryDays');
  const maxExpiryDays = Math.min(
    HARD_MAX_EXPIRY_DAYS,
    Math.max(1, Math.floor(configured ?? HARD_MAX_EXPIRY_DAYS)),
  );

  // Absent key: every known plugin. Explicit list (even empty): exactly that list.
  const raw = config.getOptional('pat.scopablePlugins');
  const scopablePlugins: ScopablePlugin[] = raw === undefined ? [...DEFAULT_SCOPABLE_PLUGINS] : [];
  if (Array.isArray(raw)) {
    for (const item of raw) {
      if (typeof item === 'string') {
        scopablePlugins.push({ id: item, label: item, description: null });
      } else if (item && typeof item === 'object' && typeof (item as any).id === 'string') {
        scopablePlugins.push({
          id: (item as any).id,
          label: typeof (item as any).label === 'string' ? (item as any).label : (item as any).id,
          description:
            typeof (item as any).description === 'string' ? (item as any).description : null,
        });
      }
    }
  }
  const retention = config.getOptionalNumber('pat.audit.retentionDays');
  const auditRetentionDays = Math.max(
    1,
    Math.floor(retention ?? DEFAULT_AUDIT_RETENTION_DAYS),
  );

  return {
    maxExpiryDays,
    auditRetentionDays,
    scopablePlugins: scopablePlugins.filter(p => p.id !== SELF_PLUGIN_ID),
  };
}

export class PatService {
  private readonly tokens: TokenStore;
  private readonly audit: AuditStore;
  private readonly logger: LoggerService;
  private readonly settings: PatSettings;
  private readonly allowedPlugins: Set<string>;

  constructor(options: {
    tokens: TokenStore;
    audit: AuditStore;
    logger: LoggerService;
    settings: PatSettings;
  }) {
    this.tokens = options.tokens;
    this.audit = options.audit;
    this.logger = options.logger;
    this.settings = options.settings;
    this.allowedPlugins = new Set(options.settings.scopablePlugins.map(p => p.id));
  }

  getSettings(): PatSettings {
    return this.settings;
  }

  async listTokens(): Promise<PatToken[]> {
    return this.tokens.list();
  }

  private validateName(value: unknown): string {
    const name = typeof value === 'string' ? value.trim() : '';
    if (!name) throw new ValidationError('name is required');
    if (name.length > NAME_MAX) {
      throw new ValidationError(`name must be at most ${NAME_MAX} characters`);
    }
    if (!NAME_PATTERN.test(name)) {
      throw new ValidationError('name may only contain letters, digits, hyphens and underscores');
    }
    return name;
  }

  private validateDescription(value: unknown): string {
    const description = typeof value === 'string' ? value.trim() : '';
    if (!description) throw new ValidationError('description is required');
    if (description.length > DESCRIPTION_MAX) {
      throw new ValidationError(`description must be at most ${DESCRIPTION_MAX} characters`);
    }
    return description;
  }

  private validateScopes(value: unknown): TokenScope[] {
    const scopes = normalizeScopes(value, this.allowedPlugins);
    if (!scopes.ok) throw new ValidationError(scopes.error);
    return scopes.scopes;
  }

  async getToken(id: string): Promise<PatToken> {
    const token = await this.tokens.get(id);
    if (!token) throw new NotFoundError('token not found');
    return token;
  }

  async createToken(input: unknown, createdBy: string): Promise<CreatedToken> {
    const body = (input ?? {}) as Record<string, unknown>;
    const name = this.validateName(body.name);
    const description = this.validateDescription(body.description);

    const expiry = resolveExpiryDays(body.expiresInDays, this.settings.maxExpiryDays);
    if (!expiry.ok) throw new ValidationError(expiry.error);

    const scopes = { scopes: this.validateScopes(body.scopes) };

    const now = new Date();
    const token = generateToken();
    const record = await this.tokens.insert(
      {
        name,
        description,
        tokenHash: hashToken(token),
        tokenPrefix: tokenPrefix(token),
        scopes: scopes.scopes,
        createdBy,
        expiresAt: addDays(now, expiry.days),
      },
      now,
    );

    await this.audit.record({
      eventType: 'token.created',
      outcome: 'allowed',
      reason: null,
      tokenId: record.id,
      tokenName: record.name,
      actor: createdBy,
      pluginId: null,
      method: null,
      path: null,
      statusCode: null,
      durationMs: null,
      ip: null,
      userAgent: null,
      details: null,
    });
    this.logger.info(
      `[pat] token created id=${record.id} name="${record.name}" by=${createdBy} expires=${record.expiresAt}`,
    );
    return { token, record };
  }

  /**
   * Edits description or scopes of an active token. Name and lifetime are
   * fixed at creation. The audit event stores the before and after values so
   * a scope widening is traceable.
   */
  async updateToken(id: string, input: unknown, updatedBy: string): Promise<PatToken> {
    const existing = await this.getToken(id);
    if (existing.state !== 'active') {
      throw new ConflictError(`cannot edit a ${existing.state} token`);
    }
    const body = (input ?? {}) as Record<string, unknown>;
    if (body.name !== undefined) {
      throw new ValidationError('name cannot be changed after creation');
    }
    const patch: UpdateTokenInput = {};
    if (body.description !== undefined) patch.description = this.validateDescription(body.description);
    if (body.scopes !== undefined) patch.scopes = this.validateScopes(body.scopes);
    if (Object.keys(patch).length === 0) {
      throw new ValidationError('nothing to update');
    }

    const updated = await this.tokens.update(id, patch);
    if (!updated) throw new NotFoundError('token not found');

    const before: Record<string, unknown> = {};
    const after: Record<string, unknown> = {};
    if (patch.description !== undefined && patch.description !== existing.description) {
      before.description = existing.description;
      after.description = updated.description;
    }
    if (patch.scopes !== undefined && JSON.stringify(patch.scopes) !== JSON.stringify(existing.scopes)) {
      before.scopes = existing.scopes;
      after.scopes = updated.scopes;
    }
    if (Object.keys(after).length === 0) {
      return updated;
    }

    await this.audit.record({
      eventType: 'token.updated',
      outcome: 'allowed',
      reason: Object.keys(after).join(','),
      tokenId: updated.id,
      tokenName: updated.name,
      actor: updatedBy,
      pluginId: null,
      method: null,
      path: null,
      statusCode: null,
      durationMs: null,
      ip: null,
      userAgent: null,
      details: JSON.stringify({ before, after }),
    });
    this.logger.info(
      `[pat] token updated id=${id} by=${updatedBy} fields=${Object.keys(after).join(',')}`,
    );
    return updated;
  }

  async revokeToken(id: string, revokedBy: string): Promise<PatToken> {
    const existing = await this.tokens.get(id);
    if (!existing) throw new NotFoundError('token not found');
    if (existing.state === 'revoked') {
      throw new ConflictError('token is already revoked');
    }
    const updated = await this.tokens.revoke(id, revokedBy);
    if (!updated) throw new ConflictError('token is already revoked');
    await this.audit.record({
      eventType: 'token.revoked',
      outcome: 'allowed',
      reason: null,
      tokenId: updated.id,
      tokenName: updated.name,
      actor: revokedBy,
      pluginId: null,
      method: null,
      path: null,
      statusCode: null,
      durationMs: null,
      ip: null,
      userAgent: null,
      details: null,
    });
    this.logger.info(`[pat] token revoked id=${id} by=${revokedBy}`);
    return updated;
  }

  /**
   * Deletes a token record. An active token stops authenticating at once,
   * so this is revoke plus removal from the list. Audit rows keep the id and
   * name, and the state at deletion is recorded as the reason.
   */
  async deleteToken(id: string, deletedBy: string): Promise<void> {
    const existing = await this.tokens.get(id);
    if (!existing) throw new NotFoundError('token not found');
    await this.tokens.delete(id);
    await this.audit.record({
      eventType: 'token.deleted',
      outcome: 'allowed',
      reason: existing.state,
      tokenId: existing.id,
      tokenName: existing.name,
      actor: deletedBy,
      pluginId: null,
      method: null,
      path: null,
      statusCode: null,
      durationMs: null,
      ip: null,
      userAgent: null,
      details: null,
    });
    this.logger.info(`[pat] token deleted id=${id} by=${deletedBy}`);
  }

  /**
   * Resolves a presented PAT for a request. Pure decision, no side effects,
   * so the gateway can record the audit event with the final status code.
   */
  async authenticate(rawToken: string, path: string, method: string): Promise<AuthenticateResult> {
    const row = await this.tokens.findByHash(hashToken(rawToken));
    if (!row) return { ok: false, reason: 'invalid_token' };
    const now = new Date();
    const token = rowToToken(row, now);
    const state = tokenState(row, now);
    if (state === 'revoked') return { ok: false, reason: 'revoked', token };
    if (state === 'expired') return { ok: false, reason: 'expired', token };

    const pluginId = pluginIdFromPath(path);
    if (!pluginId) return { ok: false, reason: 'scope_missing', token };
    const check = checkScope(token.scopes, pluginId, method);
    if (!check.allowed) return { ok: false, reason: check.reason, token };
    return { ok: true, token, scope: check.scope };
  }

  async recordUse(tokenId: string, ip: string | null): Promise<void> {
    await this.tokens.recordUse(tokenId, ip);
  }

  get auditStore(): AuditStore {
    return this.audit;
  }

  async queryAudit(q: AuditQuery): Promise<AuditPage> {
    return this.audit.query(q);
  }

  async auditSummary(windowHours = 24): Promise<AuditSummary> {
    const since = new Date(Date.now() - windowHours * 3_600_000);
    const [calls, tokens] = await Promise.all([
      this.audit.countSince(since),
      this.tokens.countByState(),
    ]);
    return {
      windowHours,
      requests: calls.requests,
      denied: calls.denied,
      activeTokens: tokens.active,
      expiringSoonTokens: tokens.expiringSoon,
    };
  }
}
