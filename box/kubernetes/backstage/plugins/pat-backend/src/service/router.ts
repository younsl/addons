import express, { Router } from 'express';
import { HttpAuthService, LoggerService } from '@backstage/backend-plugin-api';
import { Config } from '@backstage/config';
import { PatService } from './PatService';
import { AuditEventType, AuditOutcome, AuditQuery } from './types';

export interface RouterOptions {
  service: PatService;
  config: Config;
  logger: LoggerService;
  httpAuth: HttpAuthService;
}

const AUDIT_EVENT_TYPES: ReadonlySet<string> = new Set<AuditEventType>([
  'token.created',
  'token.updated',
  'token.revoked',
  'token.deleted',
  'api.request',
  'api.denied',
]);
const AUDIT_OUTCOMES: ReadonlySet<string> = new Set<AuditOutcome>(['allowed', 'denied']);
const AUDIT_MAX_LIMIT = 200;
const AUDIT_DEFAULT_LIMIT = 50;

/** Express 4 drops async rejections, so every handler is routed through here to reach the error middleware. */
function wrap(
  handler: (req: express.Request, res: express.Response) => Promise<void>,
): express.RequestHandler {
  return (req, res, next) => {
    handler(req, res).catch(next);
  };
}

function parseAuditQuery(query: express.Request['query']): AuditQuery {
  const limitRaw = Number(query.limit);
  const offsetRaw = Number(query.offset);
  const limit = Number.isInteger(limitRaw) && limitRaw > 0
    ? Math.min(limitRaw, AUDIT_MAX_LIMIT)
    : AUDIT_DEFAULT_LIMIT;
  const offset = Number.isInteger(offsetRaw) && offsetRaw >= 0 ? offsetRaw : 0;
  const eventType = typeof query.eventType === 'string' && AUDIT_EVENT_TYPES.has(query.eventType)
    ? (query.eventType as AuditEventType)
    : undefined;
  const outcome = typeof query.outcome === 'string' && AUDIT_OUTCOMES.has(query.outcome)
    ? (query.outcome as AuditOutcome)
    : undefined;
  const tokenId = typeof query.tokenId === 'string' && query.tokenId ? query.tokenId : undefined;
  const search = typeof query.search === 'string' && query.search.trim()
    ? query.search.trim().slice(0, 200)
    : undefined;
  return { limit, offset, eventType, outcome, tokenId, search };
}

export async function createRouter(options: RouterOptions): Promise<Router> {
  const { service, config, logger, httpAuth } = options;
  const admins = config.getOptionalStringArray('permission.admins') ?? [];

  /**
   * Only a signed-in user listed in `permission.admins` may manage tokens.
   *
   * An external service principal (backstage-mcp) reads with admin visibility
   * on GET and is treated as unauthenticated on every other method, so it can
   * never issue, change, revoke or delete a token. Which external tokens get
   * that far is decided before this router runs: `backend.auth.externalAccess`
   * rejects a token whose `accessRestrictions` do not list `pat`, so that
   * config block is the allowlist and there is no second one here.
   *
   * A plugin-to-plugin principal is refused outright. No backend plugin has a
   * reason to read the token inventory or the audit log, and unlike an
   * external token it carries no access restrictions, so accepting it would
   * let any plugin in this backend read credential metadata. That also covers
   * the `plugin:pat` principal the gateway mints, though a personal access
   * token cannot reach this plugin in the first place: `pat` is refused as a
   * scope target in `scopes.ts`, so the gateway rejects the call before the
   * router sees it.
   *
   * There is no unauthenticated fallback: an anonymous request is rejected
   * even when the default auth policy is disabled.
   */
  async function resolveAdmin(req: express.Request): Promise<string | undefined> {
    try {
      const credentials = await httpAuth.credentials(req as any, { allow: ['user', 'service'] });
      if (credentials.principal.type === 'service') {
        const { subject } = credentials.principal;
        if (req.method !== 'GET' || subject.startsWith('plugin:')) return undefined;
        return subject;
      }
      const ref = credentials.principal.userEntityRef;
      return admins.includes(ref) ? ref : undefined;
    } catch {
      return undefined;
    }
  }

  const adminGuard: express.RequestHandler = (req, res, next) => {
    resolveAdmin(req)
      .then(admin => {
        if (!admin) {
          res.status(403).json({ error: 'Admin only' });
          return;
        }
        (req as any).adminRef = admin;
        next();
      })
      .catch(next);
  };

  const router = Router();
  router.use(express.json());

  router.get('/health', (_req, res) => {
    res.json({ status: 'ok' });
  });

  router.get(
    '/admin-status',
    wrap(async (req, res) => {
      const admin = await resolveAdmin(req);
      res.json({ isAdmin: Boolean(admin) });
    }),
  );

  router.get('/settings', adminGuard, (_req, res) => {
    res.json(service.getSettings());
  });

  router.get(
    '/tokens',
    adminGuard,
    wrap(async (_req, res) => {
      res.json(await service.listTokens());
    }),
  );

  router.post(
    '/tokens',
    adminGuard,
    wrap(async (req, res) => {
      const created = await service.createToken(req.body, (req as any).adminRef);
      res.status(201).json(created);
    }),
  );

  router.get(
    '/tokens/:id',
    adminGuard,
    wrap(async (req, res) => {
      res.json(await service.getToken(String(req.params.id)));
    }),
  );

  router.patch(
    '/tokens/:id',
    adminGuard,
    wrap(async (req, res) => {
      const token = await service.updateToken(String(req.params.id), req.body, (req as any).adminRef);
      res.json(token);
    }),
  );

  router.post(
    '/tokens/:id/revoke',
    adminGuard,
    wrap(async (req, res) => {
      const token = await service.revokeToken(String(req.params.id), (req as any).adminRef);
      res.json(token);
    }),
  );

  router.delete(
    '/tokens/:id',
    adminGuard,
    wrap(async (req, res) => {
      await service.deleteToken(String(req.params.id), (req as any).adminRef);
      res.status(204).end();
    }),
  );

  router.get(
    '/audit',
    adminGuard,
    wrap(async (req, res) => {
      res.json(await service.queryAudit(parseAuditQuery(req.query)));
    }),
  );

  router.get(
    '/audit/summary',
    adminGuard,
    wrap(async (_req, res) => {
      res.json(await service.auditSummary());
    }),
  );

  router.use(
    (err: any, _req: express.Request, res: express.Response, _next: express.NextFunction) => {
      const status = typeof err?.statusCode === 'number' ? err.statusCode : 500;
      if (status >= 500) {
        logger.error(`[pat] request failed: ${err?.stack ?? err}`);
      }
      res.status(status).json({ error: err?.message ?? 'Internal error' });
    },
  );

  return router;
}
