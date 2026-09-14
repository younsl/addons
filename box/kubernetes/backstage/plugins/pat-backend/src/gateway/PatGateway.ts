import type { RequestHandler, Request } from 'express';
import type { LoggerService } from '@backstage/backend-plugin-api';
import { isPatToken, parseBearer } from '../service/token';
import type { DenyReason, NewAuditEvent, PatToken, TokenScope } from '../service/types';

/** Outcome of the gateway resolving a presented PAT against the request target. */
export type PatGatewayDecision =
  | { ok: true; token: PatToken; scope: TokenScope; pluginId: string; backstageToken: string }
  | { ok: false; reason: DenyReason; token?: PatToken; pluginId?: string };

/**
 * Contract between the root-level middleware and the pat backend plugin.
 * The plugin implements it once its stores and auth service are ready.
 */
export interface PatGateway {
  decide(rawToken: string, path: string, method: string): Promise<PatGatewayDecision>;
  recordUse(tokenId: string, ip: string | null): Promise<void>;
  audit(event: NewAuditEvent): Promise<void>;
}

/**
 * Holds the gateway implementation for the lifetime of the process. The
 * root HTTP router is built before any plugin initialises, so the middleware
 * is installed first and looks the implementation up per request.
 */
class PatGatewayRegistry {
  private gateway: PatGateway | undefined;

  set(gateway: PatGateway): void {
    this.gateway = gateway;
  }

  get(): PatGateway | undefined {
    return this.gateway;
  }

  clear(): void {
    this.gateway = undefined;
  }
}

export const patGatewayRegistry = new PatGatewayRegistry();

export const PAT_ID_HEADER = 'x-backstage-pat-id';
export const PAT_NAME_HEADER = 'x-backstage-pat-name';

const DENY_STATUS: Record<DenyReason, number> = {
  invalid_token: 401,
  expired: 401,
  revoked: 401,
  scope_missing: 403,
  scope_insufficient: 403,
  plugin_forbidden: 403,
  gateway_unavailable: 503,
  throttled: 429,
};

const DENY_MESSAGE: Record<DenyReason, string> = {
  invalid_token: 'Invalid personal access token',
  expired: 'Personal access token has expired',
  revoked: 'Personal access token has been revoked',
  scope_missing: 'Token has no scope for this plugin',
  scope_insufficient: 'Token scope does not allow this method',
  plugin_forbidden: 'This plugin cannot be accessed with a personal access token',
  gateway_unavailable: 'Personal access token gateway is not ready',
  throttled: 'Too many invalid personal access tokens from this client',
};

/**
 * Sliding-window counter of rejected tokens per client address. A PAT has
 * 256 bits of entropy so guessing is hopeless, but the throttle keeps a
 * misbehaving client from turning every attempt into a database lookup.
 */
export class InvalidTokenThrottle {
  private readonly hits = new Map<string, number[]>();

  constructor(
    private readonly maxAttempts = 30,
    private readonly windowMs = 60_000,
  ) {}

  /** Returns true when the client has exceeded the failure budget. */
  isBlocked(key: string, now = Date.now()): boolean {
    const list = this.prune(key, now);
    return list.length >= this.maxAttempts;
  }

  recordFailure(key: string, now = Date.now()): void {
    const list = this.prune(key, now);
    list.push(now);
    this.hits.set(key, list);
  }

  private prune(key: string, now: number): number[] {
    const list = (this.hits.get(key) ?? []).filter(t => now - t < this.windowMs);
    if (list.length === 0) this.hits.delete(key);
    else this.hits.set(key, list);
    return list;
  }
}

export function clientIp(req: Request): string | null {
  const forwarded = req.headers['x-forwarded-for'];
  const first = Array.isArray(forwarded) ? forwarded[0] : forwarded;
  if (first) return first.split(',')[0].trim() || null;
  return req.ip ?? req.socket?.remoteAddress ?? null;
}

function userAgent(req: Request): string | null {
  const ua = req.headers['user-agent'];
  return typeof ua === 'string' && ua ? ua : null;
}

/**
 * Express middleware for the root HTTP router. Requests whose bearer token
 * carries the PAT prefix are validated here and, when allowed, forwarded
 * with a plugin-to-plugin token so the target plugin sees the `plugin:pat`
 * service principal. Every decision is written to the audit log; an allowed
 * call is recorded once the response finishes so the status code is real.
 */
export function createPatGatewayMiddleware(options: {
  logger: LoggerService;
  registry?: { get(): PatGateway | undefined };
  throttle?: InvalidTokenThrottle;
}): RequestHandler {
  const { logger } = options;
  const registry = options.registry ?? patGatewayRegistry;
  const throttle = options.throttle ?? new InvalidTokenThrottle();

  return async (req, res, next) => {
    const raw = parseBearer(req.headers.authorization);
    if (!isPatToken(raw)) {
      next();
      return;
    }

    const started = Date.now();
    const gateway = registry.get();
    const path = req.originalUrl ?? req.url;
    const method = req.method.toUpperCase();
    const ip = clientIp(req);
    const ua = userAgent(req);

    const deny = async (reason: DenyReason, token?: PatToken, pluginId?: string) => {
      const status = DENY_STATUS[reason];
      res.status(status).json({ error: DENY_MESSAGE[reason], reason });
      if (!gateway) {
        logger.warn(`[pat] gateway unavailable, denied ${method} ${path}`);
        return;
      }
      try {
        await gateway.audit({
          eventType: 'api.denied',
          outcome: 'denied',
          reason,
          tokenId: token?.id ?? null,
          tokenName: token?.name ?? null,
          actor: token ? `pat:${token.id}` : null,
          pluginId: pluginId ?? null,
          method,
          path,
          statusCode: status,
          durationMs: Date.now() - started,
          ip,
          userAgent: ua,
          details: null,
        });
      } catch (err) {
        logger.warn(`[pat] failed to record denied audit event: ${err}`);
      }
    };

    if (!gateway) {
      await deny('gateway_unavailable');
      return;
    }

    const throttleKey = ip ?? 'unknown';
    if (throttle.isBlocked(throttleKey)) {
      await deny('throttled');
      return;
    }

    let decision: PatGatewayDecision;
    try {
      decision = await gateway.decide(raw, path, method);
    } catch (err) {
      logger.error(`[pat] gateway decision failed: ${err}`);
      res.status(500).json({ error: 'Personal access token validation failed' });
      return;
    }

    if (!decision.ok) {
      if (decision.reason === 'invalid_token') {
        throttle.recordFailure(throttleKey);
      }
      await deny(decision.reason, decision.token, decision.pluginId);
      return;
    }

    const { token, pluginId, backstageToken } = decision;
    req.headers.authorization = `Bearer ${backstageToken}`;
    req.headers[PAT_ID_HEADER] = token.id;
    req.headers[PAT_NAME_HEADER] = token.name;

    res.on('finish', () => {
      const statusCode = res.statusCode;
      gateway
        .audit({
          eventType: 'api.request',
          outcome: 'allowed',
          reason: null,
          tokenId: token.id,
          tokenName: token.name,
          actor: `pat:${token.id}`,
          pluginId,
          method,
          path,
          statusCode,
          durationMs: Date.now() - started,
          ip,
          userAgent: ua,
          details: null,
        })
        .catch(err => logger.warn(`[pat] failed to record audit event: ${err}`));
      gateway
        .recordUse(token.id, ip)
        .catch(err => logger.warn(`[pat] failed to record token use: ${err}`));
    });

    next();
  };
}
