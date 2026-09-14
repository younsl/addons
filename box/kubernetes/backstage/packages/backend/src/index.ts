import { createBackend } from '@backstage/backend-defaults';
import { rootHttpRouterServiceFactory } from '@backstage/backend-defaults/rootHttpRouter';
import {
  gitlabPlugin,
  catalogPluginGitlabFillerProcessorModule,
} from '@immobiliarelabs/backstage-plugin-gitlab-backend';
import { catalogModuleSonarQubeAnnotationProcessor } from './processors';
import { permissionModuleAdminPolicy } from './permissions-policy';
import { createPatGatewayMiddleware } from '@internal/plugin-pat-backend';

const backend = createBackend();

// Personal access tokens are validated before any plugin router runs. The
// middleware only acts on bearer tokens with the PAT prefix, so ordinary
// Backstage user and service tokens pass through untouched. It is installed
// ahead of applyDefaults() so the stock middleware chain stays intact across
// upgrades; the gateway throttles invalid tokens per client itself.
backend.add(
  rootHttpRouterServiceFactory({
    configure({ app, logger, applyDefaults }) {
      // The cast bridges @types/express 4 (this package) and 5 (the plugin);
      // the handler signature is identical at runtime.
      app.use(createPatGatewayMiddleware({ logger }) as any);
      applyDefaults();
    },
  }),
);

const disableGitlab = process.env.DISABLE_GITLAB === 'true';

backend.add(import('@backstage/plugin-app-backend'));
backend.add(import('@backstage/plugin-proxy-backend'));

backend.add(import('@backstage/plugin-auth-backend'));
backend.add(import('@backstage/plugin-auth-backend-module-guest-provider'));
backend.add(import('@backstage/plugin-auth-backend-module-oidc-provider'));

backend.add(import('@backstage/plugin-catalog-backend'));
backend.add(import('@backstage-community/plugin-catalog-backend-module-keycloak'));
backend.add(import('@backstage/plugin-catalog-backend-module-scaffolder-entity-model'));

if (!disableGitlab) {
  backend.add(import('@backstage/plugin-catalog-backend-module-gitlab'));
  backend.add(import('@backstage/plugin-catalog-backend-module-gitlab-org'));
}

if (!disableGitlab) {
  backend.add(gitlabPlugin);
  backend.add(catalogPluginGitlabFillerProcessorModule);
}

backend.add(catalogModuleSonarQubeAnnotationProcessor);

backend.add(import('@backstage/plugin-scaffolder-backend'));
if (!disableGitlab) {
  backend.add(import('@backstage/plugin-scaffolder-backend-module-gitlab'));
}

backend.add(import('@backstage/plugin-techdocs-backend'));

backend.add(import('@backstage/plugin-search-backend'));
backend.add(import('@backstage/plugin-search-backend-module-catalog'));

backend.add(import('@internal/plugin-platforms-backend'));

backend.add(import('@internal/plugin-openapi-registry-backend'));

backend.add(import('@internal/plugin-argocd-appset-backend'));

backend.add(import('@internal/plugin-iam-user-audit-backend'));


backend.add(import('@internal/plugin-s3-log-extract-backend'));

if (!disableGitlab) {
  backend.add(import('@internal/plugin-catalog-health-backend'));
}

backend.add(import('@internal/plugin-opencost-backend'));

backend.add(import('@internal/plugin-opensearch-account-backend'));

backend.add(import('@internal/plugin-opensearch-viewer-backend'));

backend.add(import('@internal/plugin-opensearch-scaling-backend'));

if (!disableGitlab) {
  backend.add(import('@internal/plugin-gitlab-token-audit-backend'));
}

backend.add(import('@internal/plugin-slack-mr-bot-backend'));

backend.add(import('@internal/plugin-pat-backend'));

backend.add(import('@backstage-community/plugin-sonarqube-backend'));

backend.add(import('@backstage/plugin-permission-backend'));
backend.add(permissionModuleAdminPolicy);

backend.start();
