import {
  coreServices,
  createBackendPlugin,
} from '@backstage/backend-plugin-api';
import { patGatewayRegistry, PatGateway, PatGatewayDecision } from './gateway/PatGateway';
import { AuditStore } from './service/AuditStore';
import { PatService, readSettings } from './service/PatService';
import { createRouter } from './service/router';
import { pluginIdFromPath } from './service/scopes';
import { TokenStore } from './service/TokenStore';

/**
 * Personal access tokens for external systems. Admins mint scoped, expiring
 * tokens in the UI; the root-level gateway middleware exchanges a presented
 * token for a plugin-to-plugin token and writes an audit event per call.
 */
export const patPlugin = createBackendPlugin({
  pluginId: 'pat',
  register(env) {
    env.registerInit({
      deps: {
        httpRouter: coreServices.httpRouter,
        logger: coreServices.logger,
        config: coreServices.rootConfig,
        database: coreServices.database,
        httpAuth: coreServices.httpAuth,
        auth: coreServices.auth,
        scheduler: coreServices.scheduler,
        lifecycle: coreServices.lifecycle,
      },
      async init({ httpRouter, logger, config, database, httpAuth, auth, scheduler, lifecycle }) {
        const enabled = config.getOptionalBoolean('app.plugins.pat') ?? true;
        if (!enabled) {
          logger.info('[pat] plugin disabled via app.plugins.pat');
          return;
        }

        const knex = await database.getClient();
        const tokens = await TokenStore.create({ database: knex });
        const audit = await AuditStore.create({ database: knex });
        const settings = readSettings(config);
        const service = new PatService({ tokens, audit, logger, settings });

        logger.info(
          `[pat] maxExpiryDays=${settings.maxExpiryDays} auditRetentionDays=${settings.auditRetentionDays} scopablePlugins=[${settings.scopablePlugins
            .map(p => p.id)
            .join(',')}]`,
        );

        const gateway: PatGateway = {
          async decide(rawToken, path, method): Promise<PatGatewayDecision> {
            const result = await service.authenticate(rawToken, path, method);
            const pluginId = pluginIdFromPath(path);
            if (!result.ok) {
              return { ok: false, reason: result.reason, token: result.token, pluginId };
            }
            const { token: backstageToken } = await auth.getPluginRequestToken({
              onBehalfOf: await auth.getOwnServiceCredentials(),
              targetPluginId: result.scope.plugin,
            });
            return {
              ok: true,
              token: result.token,
              scope: result.scope,
              pluginId: result.scope.plugin,
              backstageToken,
            };
          },
          recordUse: (tokenId, ip) => service.recordUse(tokenId, ip),
          audit: event => audit.record(event),
        };
        patGatewayRegistry.set(gateway);
        lifecycle.addShutdownHook(() => patGatewayRegistry.clear());

        const router = await createRouter({ service, config, logger, httpAuth });
        httpRouter.use(router as any);
        httpRouter.addAuthPolicy({ path: '/health', allow: 'unauthenticated' });

        const retentionDays = settings.auditRetentionDays;
        await scheduler.scheduleTask({
          id: 'pat-audit-purge',
          frequency: { cron: '30 3 * * *' },
          timeout: { minutes: 5 },
          initialDelay: { minutes: 2 },
          fn: async () => {
            try {
              const removed = await audit.purgeOlderThan(retentionDays);
              if (removed > 0) {
                logger.info(`[pat] purged ${removed} audit events older than ${retentionDays}d`);
              }
            } catch (err) {
              logger.error(`[pat] audit purge failed: ${err}`);
            }
          },
        });

        logger.info('[pat] plugin initialized');
      },
    });
  },
});
