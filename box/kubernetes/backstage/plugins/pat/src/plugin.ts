import {
  createApiFactory,
  createPlugin,
  createRoutableExtension,
  discoveryApiRef,
  fetchApiRef,
} from '@backstage/core-plugin-api';
import { rootRouteRef } from './routes';
import { patApiRef, PatClient } from './api';

export const patPlugin = createPlugin({
  id: 'pat',
  routes: {
    root: rootRouteRef,
  },
  apis: [
    createApiFactory({
      api: patApiRef,
      deps: {
        discoveryApi: discoveryApiRef,
        fetchApi: fetchApiRef,
      },
      factory: ({ discoveryApi, fetchApi }) => new PatClient({ discoveryApi, fetchApi }),
    }),
  ],
});

export const PatPage = patPlugin.provide(
  createRoutableExtension({
    name: 'PatPage',
    component: () => import('./components/PatPage').then(m => m.PatPage),
    mountPoint: rootRouteRef,
  }),
);
