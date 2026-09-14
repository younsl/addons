import { Knex } from 'knex';
import {
  AuditEvent,
  AuditPage,
  AuditQuery,
  NewAuditEvent,
} from './types';

const TABLE = 'pat_audit_events';

interface AuditRow {
  id: number;
  event_type: string;
  outcome: string;
  reason: string | null;
  token_id: string | null;
  token_name: string | null;
  actor: string | null;
  plugin_id: string | null;
  method: string | null;
  path: string | null;
  status_code: number | null;
  duration_ms: number | null;
  ip: string | null;
  user_agent: string | null;
  details: string | null;
  created_at: string;
}

function rowToEvent(row: AuditRow): AuditEvent {
  return {
    id: Number(row.id),
    eventType: row.event_type as AuditEvent['eventType'],
    outcome: row.outcome as AuditEvent['outcome'],
    reason: row.reason ?? null,
    tokenId: row.token_id ?? null,
    tokenName: row.token_name ?? null,
    actor: row.actor ?? null,
    pluginId: row.plugin_id ?? null,
    method: row.method ?? null,
    path: row.path ?? null,
    statusCode: row.status_code === null || row.status_code === undefined ? null : Number(row.status_code),
    durationMs: row.duration_ms === null || row.duration_ms === undefined ? null : Number(row.duration_ms),
    ip: row.ip ?? null,
    userAgent: row.user_agent ?? null,
    details: row.details ?? null,
    createdAt: row.created_at,
  };
}

const MAX_PATH_LENGTH = 1024;
const MAX_USER_AGENT_LENGTH = 512;

export class AuditStore {
  private readonly db: Knex;

  static async create(options: { database: Knex }): Promise<AuditStore> {
    const store = new AuditStore(options.database);
    await store.ensureSchema();
    return store;
  }

  private constructor(database: Knex) {
    this.db = database;
  }

  private async ensureSchema(): Promise<void> {
    const exists = await this.db.schema.hasTable(TABLE);
    if (exists) {
      // Column added after the first release. Tables created before it are
      // extended in place so no separate migration step is needed.
      if (!(await this.db.schema.hasColumn(TABLE, 'details'))) {
        await this.db.schema.alterTable(TABLE, table => {
          table.text('details');
        });
      }
      return;
    }
    await this.db.schema.createTable(TABLE, table => {
      table.increments('id').primary();
      table.string('event_type', 32).notNullable();
      table.string('outcome', 16).notNullable();
      table.string('reason', 64);
      table.string('token_id', 36);
      table.string('token_name', 100);
      table.string('actor');
      table.string('plugin_id', 64);
      table.string('method', 16);
      table.string('path', MAX_PATH_LENGTH);
      table.integer('status_code');
      table.integer('duration_ms');
      table.string('ip', 64);
      table.string('user_agent', MAX_USER_AGENT_LENGTH);
      table.text('details');
      table.string('created_at').notNullable();
      table.index(['created_at']);
      table.index(['token_id', 'created_at']);
      table.index(['event_type', 'created_at']);
    });
  }

  async record(event: NewAuditEvent, now: Date = new Date()): Promise<void> {
    await this.db(TABLE).insert({
      event_type: event.eventType,
      outcome: event.outcome,
      reason: event.reason,
      token_id: event.tokenId,
      token_name: event.tokenName,
      actor: event.actor,
      plugin_id: event.pluginId,
      method: event.method,
      path: event.path ? event.path.slice(0, MAX_PATH_LENGTH) : null,
      status_code: event.statusCode,
      duration_ms: event.durationMs,
      ip: event.ip,
      user_agent: event.userAgent ? event.userAgent.slice(0, MAX_USER_AGENT_LENGTH) : null,
      details: event.details ?? null,
      created_at: now.toISOString(),
    });
  }

  async query(q: AuditQuery): Promise<AuditPage> {
    const apply = (b: Knex.QueryBuilder) => {
      if (q.tokenId) b.where('token_id', q.tokenId);
      if (q.eventType) b.where('event_type', q.eventType);
      if (q.outcome) b.where('outcome', q.outcome);
      if (q.search) {
        const like = `%${q.search.replace(/[%_]/g, m => `\\${m}`)}%`;
        b.where(inner =>
          inner
            .whereLike('path', like)
            .orWhereLike('token_name', like)
            .orWhereLike('actor', like)
            .orWhereLike('plugin_id', like),
        );
      }
      return b;
    };
    const countRow = await apply(this.db(TABLE)).count<{ c: number | string }[]>({ c: '*' });
    const total = Number(countRow[0]?.c ?? 0);
    const rows = (await apply(this.db(TABLE))
      .orderBy('id', 'desc')
      .limit(q.limit)
      .offset(q.offset)) as AuditRow[];
    return { items: rows.map(rowToEvent), total };
  }

  async countSince(since: Date): Promise<{ requests: number; denied: number }> {
    const rows = (await this.db(TABLE)
      .where('created_at', '>=', since.toISOString())
      .whereIn('event_type', ['api.request', 'api.denied'])
      .select('event_type')
      .count({ c: '*' })
      .groupBy('event_type')) as { event_type: string; c: number | string }[];
    let requests = 0;
    let denied = 0;
    for (const r of rows) {
      const n = Number(r.c);
      if (r.event_type === 'api.request') requests += n;
      if (r.event_type === 'api.denied') denied += n;
    }
    return { requests, denied };
  }

  /** Deletes events older than `retentionDays`. Returns the number removed. */
  async purgeOlderThan(retentionDays: number, now: Date = new Date()): Promise<number> {
    const cutoff = new Date(now.getTime() - retentionDays * 86_400_000).toISOString();
    return this.db(TABLE).where('created_at', '<', cutoff).delete();
  }
}
