import express from 'express';
import request from 'supertest';
import {
  createPatGatewayMiddleware,
  InvalidTokenThrottle,
  PAT_ID_HEADER,
  PAT_NAME_HEADER,
  PatGateway,
} from './PatGateway';

const logger = {
  info: jest.fn(),
  warn: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn().mockReturnThis(),
} as any;

const token = {
  id: 'tok-1',
  name: 'jenkins',
  description: null,
  tokenPrefix: 'bs_abc',
  scopes: [{ plugin: 'catalog', access: 'read' as const }],
  state: 'active' as const,
  createdBy: 'user:default/admin',
  createdAt: '',
  expiresAt: '',
  revokedAt: null,
  revokedBy: null,
  lastUsedAt: null,
  lastUsedIp: null,
  useCount: 0,
};

function makeGateway(): jest.Mocked<PatGateway> {
  return {
    decide: jest.fn(),
    recordUse: jest.fn().mockResolvedValue(undefined),
    audit: jest.fn().mockResolvedValue(undefined),
  };
}

function makeApp(gateway: PatGateway | undefined, throttle?: InvalidTokenThrottle) {
  const app = express();
  const registry = { get: () => gateway };
  app.use(createPatGatewayMiddleware({ logger, registry, throttle }));
  app.get('/api/catalog/entities', (req, res) => {
    res.status(200).json({
      authorization: req.headers.authorization,
      patId: req.headers[PAT_ID_HEADER],
      patName: req.headers[PAT_NAME_HEADER],
    });
  });
  app.post('/api/catalog/locations', (_req, res) => res.status(201).json({}));
  return app;
}

const flush = () => new Promise(r => setImmediate(r));

describe('pat gateway middleware', () => {
  beforeEach(() => jest.clearAllMocks());

  it('passes through requests without a PAT bearer untouched', async () => {
    const gateway = makeGateway();
    const app = makeApp(gateway);
    const res = await request(app).get('/api/catalog/entities').set('Authorization', 'Bearer eyJ.jwt');
    expect(res.status).toBe(200);
    expect(res.body.authorization).toBe('Bearer eyJ.jwt');
    expect(res.body.patId).toBeUndefined();
    expect(gateway.decide).not.toHaveBeenCalled();

    const anon = await request(app).get('/api/catalog/entities');
    expect(anon.status).toBe(200);
    expect(gateway.decide).not.toHaveBeenCalled();
  });

  it('swaps an allowed PAT for the plugin token and audits the final status', async () => {
    const gateway = makeGateway();
    gateway.decide.mockResolvedValue({
      ok: true,
      token,
      scope: token.scopes[0],
      pluginId: 'catalog',
      backstageToken: 'plugin-jwt',
    });
    const res = await request(makeApp(gateway))
      .get('/api/catalog/entities?x=1')
      .set('Authorization', 'Bearer bs_secret')
      .set('X-Forwarded-For', '203.0.113.7, 10.0.0.1')
      .set('User-Agent', 'curl/8');
    expect(res.status).toBe(200);
    expect(res.body.authorization).toBe('Bearer plugin-jwt');
    expect(res.body.patId).toBe('tok-1');
    expect(res.body.patName).toBe('jenkins');
    expect(gateway.decide).toHaveBeenCalledWith('bs_secret', '/api/catalog/entities?x=1', 'GET');
    await flush();
    expect(gateway.audit).toHaveBeenCalledWith(
      expect.objectContaining({
        eventType: 'api.request',
        outcome: 'allowed',
        tokenId: 'tok-1',
        pluginId: 'catalog',
        method: 'GET',
        path: '/api/catalog/entities?x=1',
        statusCode: 200,
        ip: '203.0.113.7',
        userAgent: 'curl/8',
      }),
    );
    expect(gateway.recordUse).toHaveBeenCalledWith('tok-1', '203.0.113.7');
  });

  it.each([
    ['invalid_token', 401],
    ['expired', 401],
    ['revoked', 401],
    ['scope_missing', 403],
    ['scope_insufficient', 403],
    ['plugin_forbidden', 403],
  ] as const)('denies %s with %i and audits it', async (reason, status) => {
    const gateway = makeGateway();
    gateway.decide.mockResolvedValue({ ok: false, reason, token, pluginId: 'catalog' });
    const res = await request(makeApp(gateway))
      .post('/api/catalog/locations')
      .set('Authorization', 'Bearer bs_secret');
    expect(res.status).toBe(status);
    expect(res.body.reason).toBe(reason);
    expect(gateway.audit).toHaveBeenCalledWith(
      expect.objectContaining({
        eventType: 'api.denied',
        outcome: 'denied',
        reason,
        tokenId: 'tok-1',
        statusCode: status,
        method: 'POST',
      }),
    );
    expect(gateway.recordUse).not.toHaveBeenCalled();
  });

  it('returns 503 while the plugin has not registered the gateway', async () => {
    const res = await request(makeApp(undefined))
      .get('/api/catalog/entities')
      .set('Authorization', 'Bearer bs_secret');
    expect(res.status).toBe(503);
    expect(res.body.reason).toBe('gateway_unavailable');
  });

  it('returns 500 when the decision itself fails', async () => {
    const gateway = makeGateway();
    gateway.decide.mockRejectedValue(new Error('db down'));
    const res = await request(makeApp(gateway))
      .get('/api/catalog/entities')
      .set('Authorization', 'Bearer bs_secret');
    expect(res.status).toBe(500);
  });

  it('throttles a client after repeated invalid tokens', async () => {
    const gateway = makeGateway();
    gateway.decide.mockResolvedValue({ ok: false, reason: 'invalid_token' });
    const app = makeApp(gateway, new InvalidTokenThrottle(2, 60_000));
    const call = () =>
      request(app).get('/api/catalog/entities').set('Authorization', 'Bearer bs_bad');
    expect((await call()).status).toBe(401);
    expect((await call()).status).toBe(401);
    const third = await call();
    expect(third.status).toBe(429);
    expect(third.body.reason).toBe('throttled');
    expect(gateway.decide).toHaveBeenCalledTimes(2);
  });

  it('throttle window expires', () => {
    const t = new InvalidTokenThrottle(1, 1000);
    t.recordFailure('ip', 0);
    expect(t.isBlocked('ip', 500)).toBe(true);
    expect(t.isBlocked('ip', 1500)).toBe(false);
  });
});
