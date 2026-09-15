import { randomUUID } from 'crypto';
import { Knex } from 'knex';
import { decodeScopes, encodeScopes } from './scopes';
import { PatToken, TokenScope, TokenState } from './types';

const TABLE = 'pat_tokens';

export interface TokenRow {
  id: string;
  name: string;
  description: string | null;
  token_hash: string;
  token_prefix: string;
  scopes: string;
  created_by: string;
  created_at: string;
  expires_at: string;
  revoked_at: string | null;
  revoked_by: string | null;
  last_used_at: string | null;
  last_used_ip: string | null;
  use_count: number;
}

export interface InsertTokenInput {
  name: string;
  description: string | null;
  tokenHash: string;
  tokenPrefix: string;
  scopes: TokenScope[];
  createdBy: string;
  expiresAt: Date;
}

export function tokenState(row: Pick<TokenRow, 'revoked_at' | 'expires_at'>, now: Date): TokenState {
  if (row.revoked_at) return 'revoked';
  if (new Date(row.expires_at).getTime() <= now.getTime()) return 'expired';
  return 'active';
}

export function rowToToken(row: TokenRow, now: Date = new Date()): PatToken {
  return {
    id: row.id,
    name: row.name,
    description: row.description ?? null,
    tokenPrefix: row.token_prefix,
    scopes: decodeScopes(row.scopes),
    state: tokenState(row, now),
    createdBy: row.created_by,
    createdAt: row.created_at,
    expiresAt: row.expires_at,
    revokedAt: row.revoked_at ?? null,
    revokedBy: row.revoked_by ?? null,
    lastUsedAt: row.last_used_at ?? null,
    lastUsedIp: row.last_used_ip ?? null,
    useCount: Number(row.use_count ?? 0),
  };
}

export class TokenStore {
  private readonly db: Knex;

  static async create(options: { database: Knex }): Promise<TokenStore> {
    const store = new TokenStore(options.database);
    await store.ensureSchema();
    return store;
  }

  private constructor(database: Knex) {
    this.db = database;
  }

  private async ensureSchema(): Promise<void> {
    const exists = await this.db.schema.hasTable(TABLE);
    if (exists) return;
    await this.db.schema.createTable(TABLE, table => {
      table.string('id', 36).primary();
      table.string('name', 100).notNullable();
      table.text('description');
      table.string('token_hash', 64).notNullable().unique();
      table.string('token_prefix', 32).notNullable();
      table.text('scopes').notNullable();
      table.string('created_by').notNullable();
      table.string('created_at').notNullable();
      table.string('expires_at').notNullable();
      table.string('revoked_at');
      table.string('revoked_by');
      table.string('last_used_at');
      table.string('last_used_ip');
      table.integer('use_count').notNullable().defaultTo(0);
      table.index(['expires_at']);
    });
  }

  async insert(input: InsertTokenInput, now: Date = new Date()): Promise<PatToken> {
    const row: TokenRow = {
      id: randomUUID(),
      name: input.name,
      description: input.description,
      token_hash: input.tokenHash,
      token_prefix: input.tokenPrefix,
      scopes: encodeScopes(input.scopes),
      created_by: input.createdBy,
      created_at: now.toISOString(),
      expires_at: input.expiresAt.toISOString(),
      revoked_at: null,
      revoked_by: null,
      last_used_at: null,
      last_used_ip: null,
      use_count: 0,
    };
    await this.db(TABLE).insert(row);
    return rowToToken(row, now);
  }

  async list(): Promise<PatToken[]> {
    const rows = (await this.db(TABLE).orderBy('created_at', 'desc')) as TokenRow[];
    const now = new Date();
    return rows.map(r => rowToToken(r, now));
  }

  async get(id: string): Promise<PatToken | undefined> {
    const row = (await this.db(TABLE).where({ id }).first()) as TokenRow | undefined;
    return row ? rowToToken(row) : undefined;
  }

  /** Raw lookup by hash for the gateway. Returns the row so callers can check state themselves. */
  async findByHash(tokenHash: string): Promise<TokenRow | undefined> {
    return (await this.db(TABLE).where({ token_hash: tokenHash }).first()) as
      | TokenRow
      | undefined;
  }

  async revoke(id: string, revokedBy: string, now: Date = new Date()): Promise<PatToken | undefined> {
    const updated = await this.db(TABLE)
      .where({ id })
      .whereNull('revoked_at')
      .update({ revoked_at: now.toISOString(), revoked_by: revokedBy });
    if (updated === 0) return undefined;
    return this.get(id);
  }

  async update(
    id: string,
    patch: { description?: string; scopes?: TokenScope[] },
  ): Promise<PatToken | undefined> {
    const values: Partial<TokenRow> = {};
    if (patch.description !== undefined) values.description = patch.description;
    if (patch.scopes !== undefined) values.scopes = encodeScopes(patch.scopes);
    if (Object.keys(values).length > 0) {
      const updated = await this.db(TABLE).where({ id }).update(values);
      if (updated === 0) return undefined;
    }
    return this.get(id);
  }

  async delete(id: string): Promise<boolean> {
    const deleted = await this.db(TABLE).where({ id }).delete();
    return deleted > 0;
  }

  async recordUse(id: string, ip: string | null, now: Date = new Date()): Promise<void> {
    await this.db(TABLE)
      .where({ id })
      .update({
        last_used_at: now.toISOString(),
        last_used_ip: ip,
        use_count: this.db.raw('use_count + 1'),
      });
  }

  async countByState(now: Date = new Date(), expiringWithinDays = 30): Promise<{ active: number; expiringSoon: number }> {
    const rows = (await this.db(TABLE)
      .whereNull('revoked_at')
      .select('expires_at')) as Pick<TokenRow, 'expires_at'>[];
    const soon = now.getTime() + expiringWithinDays * 86_400_000;
    let active = 0;
    let expiringSoon = 0;
    for (const r of rows) {
      const exp = new Date(r.expires_at).getTime();
      if (exp <= now.getTime()) continue;
      active += 1;
      if (exp <= soon) expiringSoon += 1;
    }
    return { active, expiringSoon };
  }
}
