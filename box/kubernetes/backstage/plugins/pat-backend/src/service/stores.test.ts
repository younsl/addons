import knex, { Knex } from 'knex';
import { AuditStore } from './AuditStore';
import { TokenStore } from './TokenStore';
import { NewAuditEvent } from './types';

const apiEvent = (overrides: Partial<NewAuditEvent> = {}): NewAuditEvent => ({
  eventType: 'api.request',
  outcome: 'allowed',
  reason: null,
  tokenId: 't1',
  tokenName: 'jenkins',
  actor: 'pat:t1',
  pluginId: 'catalog',
  method: 'GET',
  path: '/api/catalog/entities',
  statusCode: 200,
  durationMs: 12,
  ip: '10.0.0.1',
  userAgent: 'curl',
  details: null,
  ...overrides,
});

describe('stores', () => {
  let db: Knex;

  beforeEach(() => {
    db = knex({
      client: 'better-sqlite3',
      connection: { filename: ':memory:' },
      useNullAsDefault: true,
    });
  });

  afterEach(async () => {
    await db.destroy();
  });

  describe('TokenStore', () => {
    it('inserts, lists and derives state', async () => {
      const store = await TokenStore.create({ database: db });
      const now = new Date('2026-01-01T00:00:00Z');
      const active = await store.insert(
        {
          name: 'a',
          description: 'd',
          tokenHash: 'h1',
          tokenPrefix: 'bs_aaaa',
          scopes: [{ plugin: 'catalog', access: 'read' }],
          createdBy: 'user:default/admin',
          expiresAt: new Date('2026-12-31T00:00:00Z'),
        },
        now,
      );
      expect(active.state).toBe('active');
      expect(active.scopes).toEqual([{ plugin: 'catalog', access: 'read' }]);

      const list = await store.list();
      expect(list).toHaveLength(1);
      expect((list[0] as any).tokenHash).toBeUndefined();
    });

    it('marks past expiry as expired', async () => {
      const store = await TokenStore.create({ database: db });
      await store.insert({
        name: 'old',
        description: 'd',
        tokenHash: 'h2',
        tokenPrefix: 'bs_bbbb',
        scopes: [{ plugin: 'catalog', access: 'read' }],
        createdBy: 'user:default/admin',
        expiresAt: new Date(Date.now() - 1000),
      });
      const [row] = await store.list();
      expect(row.state).toBe('expired');
    });

    it('revokes once and refuses a second time', async () => {
      const store = await TokenStore.create({ database: db });
      const t = await store.insert({
        name: 'r',
        description: 'd',
        tokenHash: 'h3',
        tokenPrefix: 'bs_cccc',
        scopes: [{ plugin: 'catalog', access: 'read' }],
        createdBy: 'user:default/admin',
        expiresAt: new Date(Date.now() + 86_400_000),
      });
      const revoked = await store.revoke(t.id, 'user:default/admin');
      expect(revoked?.state).toBe('revoked');
      expect(revoked?.revokedBy).toBe('user:default/admin');
      expect(await store.revoke(t.id, 'user:default/other')).toBeUndefined();
    });

    it('records use and finds by hash', async () => {
      const store = await TokenStore.create({ database: db });
      const t = await store.insert({
        name: 'u',
        description: 'd',
        tokenHash: 'h4',
        tokenPrefix: 'bs_dddd',
        scopes: [{ plugin: 'catalog', access: 'read' }],
        createdBy: 'user:default/admin',
        expiresAt: new Date(Date.now() + 86_400_000),
      });
      await store.recordUse(t.id, '10.1.1.1');
      await store.recordUse(t.id, '10.1.1.2');
      const found = await store.findByHash('h4');
      expect(found?.use_count).toBe(2);
      expect(found?.last_used_ip).toBe('10.1.1.2');
      expect(await store.findByHash('nope')).toBeUndefined();
    });

    it('counts active and expiring tokens', async () => {
      const store = await TokenStore.create({ database: db });
      const base = {
        description: 'd',
        scopes: [{ plugin: 'catalog', access: 'read' as const }],
        createdBy: 'user:default/admin',
      };
      await store.insert({ ...base, name: 'soon', tokenHash: 'a', tokenPrefix: 'p', expiresAt: new Date(Date.now() + 5 * 86_400_000) });
      await store.insert({ ...base, name: 'far', tokenHash: 'b', tokenPrefix: 'p', expiresAt: new Date(Date.now() + 200 * 86_400_000) });
      await store.insert({ ...base, name: 'gone', tokenHash: 'c', tokenPrefix: 'p', expiresAt: new Date(Date.now() - 1) });
      expect(await store.countByState()).toEqual({ active: 2, expiringSoon: 1 });
    });

    it('is idempotent on schema creation', async () => {
      await TokenStore.create({ database: db });
      await expect(TokenStore.create({ database: db })).resolves.toBeDefined();
    });

    it('updates name, description and scopes', async () => {
      const store = await TokenStore.create({ database: db });
      const t = await store.insert({
        name: 'n',
        description: 'd',
        tokenHash: 'h5',
        tokenPrefix: 'p',
        scopes: [{ plugin: 'catalog', access: 'read' }],
        createdBy: 'user:default/admin',
        expiresAt: new Date(Date.now() + 86_400_000),
      });
      const updated = await store.update(t.id, {
        description: 'd2',
        scopes: [{ plugin: 'catalog', access: 'write' }],
      });
      expect(updated?.name).toBe('n');
      expect(updated?.description).toBe('d2');
      expect(updated?.scopes).toEqual([{ plugin: 'catalog', access: 'write' }]);
      expect(await store.update('missing', { name: 'x' })).toBeUndefined();
    });
  });

  describe('AuditStore', () => {
    it('records and pages events newest first', async () => {
      const store = await AuditStore.create({ database: db });
      for (let i = 0; i < 5; i += 1) {
        await store.record(apiEvent({ path: `/api/catalog/${i}` }));
      }
      const page = await store.query({ limit: 2, offset: 0 });
      expect(page.total).toBe(5);
      expect(page.items.map(e => e.path)).toEqual(['/api/catalog/4', '/api/catalog/3']);
      const next = await store.query({ limit: 2, offset: 4 });
      expect(next.items).toHaveLength(1);
    });

    it('filters by token, type, outcome and search', async () => {
      const store = await AuditStore.create({ database: db });
      await store.record(apiEvent());
      await store.record(apiEvent({ tokenId: 't2', tokenName: 'argo', eventType: 'api.denied', outcome: 'denied', reason: 'expired', statusCode: 401 }));
      await store.record({ ...apiEvent(), eventType: 'token.created', method: null, path: null, pluginId: null, statusCode: null, durationMs: null, actor: 'user:default/admin' });

      expect((await store.query({ limit: 10, offset: 0, tokenId: 't2' })).total).toBe(1);
      expect((await store.query({ limit: 10, offset: 0, eventType: 'token.created' })).total).toBe(1);
      expect((await store.query({ limit: 10, offset: 0, outcome: 'denied' })).total).toBe(1);
      expect((await store.query({ limit: 10, offset: 0, search: 'argo' })).total).toBe(1);
      expect((await store.query({ limit: 10, offset: 0, search: 'entities' })).total).toBe(2);
    });

    it('summarises calls since a point in time', async () => {
      const store = await AuditStore.create({ database: db });
      await store.record(apiEvent());
      await store.record(apiEvent({ eventType: 'api.denied', outcome: 'denied', reason: 'invalid_token' }));
      await store.record(apiEvent(), new Date(Date.now() - 3 * 86_400_000));
      const counts = await store.countSince(new Date(Date.now() - 86_400_000));
      expect(counts).toEqual({ requests: 1, denied: 1 });
    });

    it('purges events past retention', async () => {
      const store = await AuditStore.create({ database: db });
      await store.record(apiEvent(), new Date(Date.now() - 100 * 86_400_000));
      await store.record(apiEvent());
      expect(await store.purgeOlderThan(90)).toBe(1);
      expect((await store.query({ limit: 10, offset: 0 })).total).toBe(1);
    });

    it('adds the details column to a pre-existing table', async () => {
      await db.schema.createTable('pat_audit_events', table => {
        table.increments('id').primary();
        table.string('event_type', 32).notNullable();
        table.string('outcome', 16).notNullable();
        table.string('reason', 64);
        table.string('token_id', 36);
        table.string('token_name', 100);
        table.string('actor');
        table.string('plugin_id', 64);
        table.string('method', 16);
        table.string('path', 1024);
        table.integer('status_code');
        table.integer('duration_ms');
        table.string('ip', 64);
        table.string('user_agent', 512);
        table.string('created_at').notNullable();
      });
      const store = await AuditStore.create({ database: db });
      await store.record(apiEvent({ eventType: 'token.updated', details: '{"a":1}' }));
      const [e] = (await store.query({ limit: 1, offset: 0 })).items;
      expect(e.details).toBe('{"a":1}');
    });

    it('truncates oversized path and user agent', async () => {
      const store = await AuditStore.create({ database: db });
      await store.record(apiEvent({ path: `/api/catalog/${'x'.repeat(2000)}`, userAgent: 'u'.repeat(1000) }));
      const [e] = (await store.query({ limit: 1, offset: 0 })).items;
      expect(e.path!.length).toBe(1024);
      expect(e.userAgent!.length).toBe(512);
    });
  });
});
