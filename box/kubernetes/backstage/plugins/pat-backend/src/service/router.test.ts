import express from 'express';
import request from 'supertest';
import { ConfigReader } from '@backstage/config';
import { createRouter } from './router';
import { ConflictError, NotFoundError, ValidationError } from './PatService';

const logger = {
  info: jest.fn(),
  warn: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn().mockReturnThis(),
} as any;

const service = {
  getToken: jest.fn(),
  updateToken: jest.fn(),
  getSettings: jest.fn().mockReturnValue({ maxExpiryDays: 365, auditRetentionDays: 365, scopablePlugins: [] }),
  listTokens: jest.fn().mockResolvedValue([]),
  createToken: jest.fn(),
  revokeToken: jest.fn(),
  deleteToken: jest.fn(),
  queryAudit: jest.fn().mockResolvedValue({ items: [], total: 0 }),
  auditSummary: jest.fn().mockResolvedValue({ windowHours: 24, requests: 0, denied: 0, activeTokens: 0, expiringSoonTokens: 0 }),
};

const httpAuth = { credentials: jest.fn() };

const asUser = (ref: string) =>
  httpAuth.credentials.mockResolvedValue({ principal: { type: 'user', userEntityRef: ref } });
const asService = (subject = 'backstage-mcp') =>
  httpAuth.credentials.mockResolvedValue({ principal: { type: 'service', subject } });
const asAnonymous = () => httpAuth.credentials.mockRejectedValue(new Error('missing credentials'));

async function makeApp() {
  const config = new ConfigReader({
    permission: { admins: ['user:default/admin'] },
    backend: { auth: { dangerouslyDisableDefaultAuthPolicy: true } },
  });
  const router = await createRouter({
    service: service as any,
    config,
    logger,
    httpAuth: httpAuth as any,
  });
  const app = express();
  app.use(router);
  return app;
}

describe('pat router', () => {
  beforeEach(() => jest.clearAllMocks());

  it('serves health without auth', async () => {
    asAnonymous();
    const res = await request(await makeApp()).get('/health');
    expect(res.status).toBe(200);
  });

  it('reports admin status', async () => {
    const app = await makeApp();
    asUser('user:default/admin');
    expect((await request(app).get('/admin-status')).body).toEqual({ isAdmin: true });
    asUser('user:default/bob');
    expect((await request(app).get('/admin-status')).body).toEqual({ isAdmin: false });
    asAnonymous();
    expect((await request(app).get('/admin-status')).body).toEqual({ isAdmin: false });
  });

  it('denies non-admins and anonymous callers on every admin route', async () => {
    const app = await makeApp();
    for (const setup of [() => asUser('user:default/bob'), asAnonymous]) {
      setup();
      expect((await request(app).get('/tokens')).status).toBe(403);
      expect((await request(app).post('/tokens').send({})).status).toBe(403);
      expect((await request(app).post('/tokens/x/revoke')).status).toBe(403);
      expect((await request(app).get('/tokens/x')).status).toBe(403);
      expect((await request(app).patch('/tokens/x').send({})).status).toBe(403);
      expect((await request(app).delete('/tokens/x')).status).toBe(403);
      expect((await request(app).get('/audit')).status).toBe(403);
      expect((await request(app).get('/audit/summary')).status).toBe(403);
      expect((await request(app).get('/settings')).status).toBe(403);
    }
    expect(service.createToken).not.toHaveBeenCalled();
  });

  it('refuses a plugin-to-plugin principal outright', async () => {
    const app = await makeApp();
    asService('plugin:catalog');
    expect((await request(app).get('/tokens')).status).toBe(403);
    expect((await request(app).get('/audit')).status).toBe(403);
    expect((await request(app).get('/settings')).status).toBe(403);
    // The gateway mints this one for forwarded PAT calls. `scopes.ts` already
    // refuses `pat` as a scope target, so it must never read here either.
    asService('plugin:pat');
    expect((await request(app).get('/tokens')).status).toBe(403);
    expect((await request(app).get('/audit')).status).toBe(403);
  });

  it('lets an external service principal read but never write', async () => {
    const app = await makeApp();
    asService();
    service.getToken.mockResolvedValue({ id: 'abc' });
    expect((await request(app).get('/tokens')).status).toBe(200);
    expect((await request(app).get('/tokens/abc')).status).toBe(200);
    expect((await request(app).get('/audit')).status).toBe(200);
    expect((await request(app).get('/audit/summary')).status).toBe(200);
    expect((await request(app).get('/settings')).status).toBe(200);

    expect((await request(app).post('/tokens').send({})).status).toBe(403);
    expect((await request(app).patch('/tokens/abc').send({})).status).toBe(403);
    expect((await request(app).post('/tokens/abc/revoke')).status).toBe(403);
    expect((await request(app).delete('/tokens/abc')).status).toBe(403);
    expect(service.createToken).not.toHaveBeenCalled();
    expect(service.updateToken).not.toHaveBeenCalled();
    expect(service.revokeToken).not.toHaveBeenCalled();
    expect(service.deleteToken).not.toHaveBeenCalled();
  });

  it('lets admins create tokens and passes the actor', async () => {
    asUser('user:default/admin');
    service.createToken.mockResolvedValue({ token: 'bs_x', record: { id: '1' } });
    const res = await request(await makeApp())
      .post('/tokens')
      .send({ name: 'n', description: 'd', expiresInDays: 1, scopes: [] });
    expect(res.status).toBe(201);
    expect(res.body.token).toBe('bs_x');
    expect(service.createToken).toHaveBeenCalledWith(
      { name: 'n', description: 'd', expiresInDays: 1, scopes: [] },
      'user:default/admin',
    );
  });

  it('maps service errors to status codes', async () => {
    asUser('user:default/admin');
    const app = await makeApp();
    service.createToken.mockRejectedValue(new ValidationError('bad'));
    expect((await request(app).post('/tokens').send({})).status).toBe(400);
    service.revokeToken.mockRejectedValue(new NotFoundError('nope'));
    expect((await request(app).post('/tokens/x/revoke')).status).toBe(404);
    service.deleteToken.mockRejectedValue(new ConflictError('active'));
    expect((await request(app).delete('/tokens/x')).status).toBe(409);
    service.listTokens.mockRejectedValueOnce(new Error('boom'));
    expect((await request(app).get('/tokens')).status).toBe(500);
  });

  it('reads and patches a single token', async () => {
    asUser('user:default/admin');
    service.getToken.mockResolvedValue({ id: 'abc' });
    service.updateToken.mockResolvedValue({ id: 'abc', description: 'new' });
    const app = await makeApp();
    expect((await request(app).get('/tokens/abc')).body).toEqual({ id: 'abc' });
    const res = await request(app).patch('/tokens/abc').send({ description: 'new' });
    expect(res.status).toBe(200);
    expect(service.updateToken).toHaveBeenCalledWith('abc', { description: 'new' }, 'user:default/admin');
  });

  it('returns 204 on delete', async () => {
    asUser('user:default/admin');
    service.deleteToken.mockResolvedValue(undefined);
    expect((await request(await makeApp()).delete('/tokens/abc')).status).toBe(204);
    expect(service.deleteToken).toHaveBeenCalledWith('abc', 'user:default/admin');
  });

  it('sanitises audit query parameters', async () => {
    asUser('user:default/admin');
    const app = await makeApp();
    await request(app).get('/audit?limit=5000&offset=-3&eventType=bogus&outcome=denied&tokenId=t1&search=%20abc%20');
    expect(service.queryAudit).toHaveBeenCalledWith({
      limit: 200,
      offset: 0,
      eventType: undefined,
      outcome: 'denied',
      tokenId: 't1',
      search: 'abc',
    });
    await request(app).get('/audit');
    expect(service.queryAudit).toHaveBeenLastCalledWith({
      limit: 50,
      offset: 0,
      eventType: undefined,
      outcome: undefined,
      tokenId: undefined,
      search: undefined,
    });
  });
});
