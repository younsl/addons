import { createBackend } from '@backstage/backend-defaults';
import { permissionModuleAdminPolicy } from './permissions-policy';

const backend = createBackend();

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

backend.add(import('@backstage/plugin-scaffolder-backend'));
if (!disableGitlab) {
  backend.add(import('@backstage/plugin-scaffolder-backend-module-gitlab'));
}

backend.add(import('@backstage/plugin-techdocs-backend'));

backend.add(import('@backstage/plugin-search-backend'));
backend.add(import('@backstage/plugin-search-backend-module-catalog'));

backend.add(import('@internal/plugin-platforms-backend'));

backend.add(import('@internal/plugin-argocd-appset-backend'));

backend.add(import('@internal/plugin-opencost-backend'));

backend.add(import('@backstage/plugin-permission-backend'));
backend.add(permissionModuleAdminPolicy);

backend.start();
