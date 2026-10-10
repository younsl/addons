import { createPermission } from '@backstage/plugin-permission-common';

export const argocdAppsetMutePermission = createPermission({
  name: 'argocd.appset.mute',
  attributes: { action: 'update' },
});
