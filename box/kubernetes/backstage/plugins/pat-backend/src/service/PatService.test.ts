import knex, { Knex } from 'knex';
import { ConfigReader } from '@backstage/config';
import { AuditStore } from './AuditStore';
import { DEFAULT_SCOPABLE_PLUGINS } from './defaults';
import { PatService, readSettings } from './PatService';
import { TokenStore } from './TokenStore';

const logger = {
  info: jest.fn(),
  warn: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn().mockReturnThis(),
} as any;

const settings = {
  maxExpiryDays: 365,
  auditRetentionDays: 365,
  scopablePlugins: [
    { id: 'catalog', label: 'Catalog', description: null },
    { id: 'opencost', label: 'OpenCost', description: null },
  ],
};

const validInput = {
  name: 'jenkins',
  description: 'Catalog sync from Jenkins',
  expiresInDays: 30,
  scopes: [
    { plugin: 'catalog', access: 'read' },
    { plugin: 'opencost', access: 'write' },
  ],
};

describe('PatService', () => {
  let db: Knex;
  let service: PatService;
  let audit: AuditStore;

  beforeEach(async () => {
    db = knex({
      client: 'better-sqlite3',
      connection: { filename: ':memory:' },
      useNullAsDefault: true,
    });
    const tokens = await TokenStore.create({ database: db });
    audit = await AuditStore.create({ database: db });
    service = new PatService({ tokens, audit, logger, settings });
  });

  afterEach(async () => {
    await db.destroy();
  });

  describe('readSettings', () => {
    it('caps maxExpiryDays at 365 and drops the pat plugin', () => {
      const config = new ConfigReader({
        pat: {
          maxExpiryDays: 9999,
          audit: { retentionDays: 30 },
          scopablePlugins: ['catalog', { id: 'pat' }, { id: 'search', label: 'Search', description: 'x' }],
        },
      });
      expect(readSettings(config)).toEqual({
        maxExpiryDays: 365,
        auditRetentionDays: 30,
        scopablePlugins: [
          { id: 'catalog', label: 'catalog', description: null },
          { id: 'search', label: 'Search', description: 'x' },
        ],
      });
    });

    it('defaults when unset', () => {
      expect(readSettings(new ConfigReader({}))).toEqual({
        maxExpiryDays: 365,
        auditRetentionDays: 365,
        scopablePlugins: DEFAULT_SCOPABLE_PLUGINS,
      });
      expect(DEFAULT_SCOPABLE_PLUGINS.map(p => p.id)).not.toContain('pat');
    });

    it('exposes nothing when scopablePlugins is an explicit empty list', () => {
      const config = new ConfigReader({ pat: { scopablePlugins: [] } });
      expect(readSettings(config).scopablePlugins).toEqual([]);
    });
  });

  describe('createToken', () => {
    it('returns the secret once and stores only a hash', async () => {
      const created = await service.createToken(validInput, 'user:default/admin');
      expect(created.token).toMatch(/^bs_/);
      expect(created.record.name).toBe('jenkins');
      expect(created.record.state).toBe('active');
      expect(created.record.scopes).toEqual([
        { plugin: 'catalog', access: 'read' },
        { plugin: 'opencost', access: 'write' },
      ]);
      const rows = await db('pat_tokens');
      expect(rows[0].token_hash).not.toContain(created.token);
      const events = await audit.query({ limit: 10, offset: 0 });
      expect(events.items[0].eventType).toBe('token.created');
      expect(events.items[0].actor).toBe('user:default/admin');
    });

    it.each([
      ['missing name', { ...validInput, name: '  ' }],
      ['name with spaces', { ...validInput, name: 'jenkins sync' }],
      ['name with symbols', { ...validInput, name: 'jenkins/sync!' }],
      ['name with unicode', { ...validInput, name: '젠킨스' }],
      ['missing description', { ...validInput, description: '' }],
      ['too long', { ...validInput, expiresInDays: 366 }],
      ['zero days', { ...validInput, expiresInDays: 0 }],
      ['no scopes', { ...validInput, scopes: [] }],
      ['unknown plugin', { ...validInput, scopes: [{ plugin: 'scaffolder', access: 'read' }] }],
      ['self scope', { ...validInput, scopes: [{ plugin: 'pat', access: 'read' }] }],
    ])('rejects %s', async (_label, input) => {
      await expect(service.createToken(input, 'user:default/admin')).rejects.toMatchObject({
        statusCode: 400,
      });
    });
  });

  describe('authenticate', () => {
    it('allows a matching read scope on GET', async () => {
      const { token } = await service.createToken(validInput, 'user:default/admin');
      const r = await service.authenticate(token, '/api/catalog/entities', 'GET');
      expect(r.ok).toBe(true);
      if (r.ok) expect(r.scope).toEqual({ plugin: 'catalog', access: 'read' });
    });

    it('denies POST on a read scope', async () => {
      const { token } = await service.createToken(validInput, 'user:default/admin');
      expect(await service.authenticate(token, '/api/catalog/locations', 'POST')).toMatchObject({
        ok: false,
        reason: 'scope_insufficient',
      });
    });

    it('allows POST on a write scope', async () => {
      const { token } = await service.createToken(validInput, 'user:default/admin');
      expect((await service.authenticate(token, '/api/opencost/x', 'POST')).ok).toBe(true);
    });

    it('denies plugins without scope and the pat plugin', async () => {
      const { token } = await service.createToken(validInput, 'user:default/admin');
      expect(await service.authenticate(token, '/api/search/query', 'GET')).toMatchObject({
        ok: false,
        reason: 'scope_missing',
      });
      expect(await service.authenticate(token, '/api/pat/tokens', 'GET')).toMatchObject({
        ok: false,
        reason: 'plugin_forbidden',
      });
    });

    it('denies unknown tokens', async () => {
      expect(await service.authenticate('bs_nope', '/api/catalog/entities', 'GET')).toEqual({
        ok: false,
        reason: 'invalid_token',
      });
    });

    it('denies revoked tokens', async () => {
      const { token, record } = await service.createToken(validInput, 'user:default/admin');
      await service.revokeToken(record.id, 'user:default/admin');
      expect(await service.authenticate(token, '/api/catalog/entities', 'GET')).toMatchObject({
        ok: false,
        reason: 'revoked',
      });
    });

    it('denies expired tokens', async () => {
      const { token, record } = await service.createToken(validInput, 'user:default/admin');
      await db('pat_tokens')
        .where({ id: record.id })
        .update({ expires_at: new Date(Date.now() - 1000).toISOString() });
      expect(await service.authenticate(token, '/api/catalog/entities', 'GET')).toMatchObject({
        ok: false,
        reason: 'expired',
      });
    });
  });

  describe('revoke and delete', () => {
    it('revokes once, then refuses', async () => {
      const { record } = await service.createToken(validInput, 'user:default/admin');
      const revoked = await service.revokeToken(record.id, 'user:default/admin');
      expect(revoked.state).toBe('revoked');
      await expect(service.revokeToken(record.id, 'user:default/admin')).rejects.toMatchObject({
        statusCode: 409,
      });
      await expect(service.revokeToken('missing', 'user:default/admin')).rejects.toMatchObject({
        statusCode: 404,
      });
    });

    it('deletes an active token and records its state', async () => {
      const { token, record } = await service.createToken(validInput, 'user:default/admin');
      await service.deleteToken(record.id, 'user:default/admin');
      expect(await service.listTokens()).toHaveLength(0);
      expect(await service.authenticate(token, '/api/catalog/entities', 'GET')).toEqual({
        ok: false,
        reason: 'invalid_token',
      });
      const events = await audit.query({ limit: 10, offset: 0 });
      expect(events.items.map(e => e.eventType)).toEqual(['token.deleted', 'token.created']);
      expect(events.items[0].reason).toBe('active');
      await expect(service.deleteToken(record.id, 'user:default/admin')).rejects.toMatchObject({
        statusCode: 404,
      });
    });
  });

  describe('updateToken', () => {
    it('changes scopes and description and records a diff', async () => {
      const { token, record } = await service.createToken(validInput, 'user:default/admin');
      const updated = await service.updateToken(
        record.id,
        { description: 'Now also writes catalog', scopes: [{ plugin: 'catalog', access: 'write' }] },
        'user:default/admin',
      );
      expect(updated.description).toBe('Now also writes catalog');
      expect(updated.scopes).toEqual([{ plugin: 'catalog', access: 'write' }]);
      expect((await service.authenticate(token, '/api/catalog/locations', 'POST')).ok).toBe(true);
      expect(await service.authenticate(token, '/api/opencost/x', 'GET')).toMatchObject({
        ok: false,
        reason: 'scope_missing',
      });
      const [event] = (await audit.query({ limit: 1, offset: 0 })).items;
      expect(event.eventType).toBe('token.updated');
      expect(event.reason).toBe('description,scopes');
      expect(JSON.parse(event.details!)).toEqual({
        before: { description: validInput.description, scopes: validInput.scopes },
        after: { description: 'Now also writes catalog', scopes: [{ plugin: 'catalog', access: 'write' }] },
      });
    });

    it('refuses to rename a token, even to its current name', async () => {
      const { record } = await service.createToken(validInput, 'user:default/admin');
      for (const name of ['renamed', validInput.name]) {
        await expect(
          service.updateToken(record.id, { name }, 'user:default/admin'),
        ).rejects.toMatchObject({ statusCode: 400, message: expect.stringContaining('name') });
      }
      expect((await service.getToken(record.id)).name).toBe(validInput.name);
      const events = await audit.query({ limit: 10, offset: 0 });
      expect(events.items.map(e => e.eventType)).toEqual(['token.created']);
    });

    it('does not write an audit event when nothing changed', async () => {
      const { record } = await service.createToken(validInput, 'user:default/admin');
      await service.updateToken(
        record.id,
        { description: validInput.description },
        'user:default/admin',
      );
      const events = await audit.query({ limit: 10, offset: 0 });
      expect(events.items.map(e => e.eventType)).toEqual(['token.created']);
    });

    it('rejects empty patches, invalid values and non-active tokens', async () => {
      const { record } = await service.createToken(validInput, 'user:default/admin');
      await expect(service.updateToken(record.id, {}, 'user:default/admin')).rejects.toMatchObject({
        statusCode: 400,
      });
      await expect(
        service.updateToken(record.id, { scopes: [] }, 'user:default/admin'),
      ).rejects.toMatchObject({ statusCode: 400 });
      await expect(
        service.updateToken(record.id, { description: '' }, 'user:default/admin'),
      ).rejects.toMatchObject({ statusCode: 400 });
      await service.revokeToken(record.id, 'user:default/admin');
      await expect(
        service.updateToken(record.id, { description: 'x' }, 'user:default/admin'),
      ).rejects.toMatchObject({ statusCode: 409 });
      await expect(
        service.updateToken('missing', { description: 'x' }, 'user:default/admin'),
      ).rejects.toMatchObject({ statusCode: 404 });
    });

    it('returns a single token and 404s on unknown ids', async () => {
      const { record } = await service.createToken(validInput, 'user:default/admin');
      expect((await service.getToken(record.id)).id).toBe(record.id);
      await expect(service.getToken('missing')).rejects.toMatchObject({ statusCode: 404 });
    });
  });

  it('summarises audit and token counts', async () => {
    await service.createToken({ ...validInput, expiresInDays: 90 }, 'user:default/admin');
    await service.createToken({ ...validInput, name: 'soon', expiresInDays: 7 }, 'user:default/admin');
    await audit.record({
      eventType: 'api.denied',
      outcome: 'denied',
      reason: 'invalid_token',
      tokenId: null,
      tokenName: null,
      actor: null,
      pluginId: 'catalog',
      method: 'GET',
      path: '/api/catalog',
      statusCode: 401,
      durationMs: 1,
      ip: null,
      userAgent: null,
    });
    expect(await service.auditSummary()).toEqual({
      windowHours: 24,
      requests: 0,
      denied: 1,
      activeTokens: 2,
      expiringSoonTokens: 1,
    });
  });
});
