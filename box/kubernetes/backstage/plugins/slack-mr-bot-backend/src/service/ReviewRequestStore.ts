import { Knex } from 'knex';

const REQUESTS_TABLE = 'slack_mr_bot_requests';
const APPROVALS_TABLE = 'slack_mr_bot_approvals';

/** Why a request is no longer polled. `opened` is the only live status. */
export type TrackedStatus =
  | 'opened'
  | 'merged'
  | 'closed'
  | 'expired'
  | 'unreachable';

/** One merge request line of one posted review request message. */
export interface TrackedRequest {
  id: number;
  url: string;
  reference: string;
  channel: string;
  threadTs: string;
  requester: string;
  status: TrackedStatus;
  failureCount: number;
  createdAt: string;
}

export interface PostedMessage {
  channel: string;
  /** `ts` of the review request message; replies thread under it. */
  threadTs: string;
  /** Slack user id of whoever ran the command. */
  requester: string;
}

export interface ReviewRequestStoreOptions {
  database: Knex;
}

/**
 * Normalizes whatever the driver hands back for a timestamp column. Postgres
 * returns a Date, SQLite the string it was given, and either can return an
 * epoch in milliseconds depending on how the value was written.
 */
export function toIsoStamp(value: unknown): string {
  if (value instanceof Date) return value.toISOString();
  if (typeof value === 'number') return new Date(value).toISOString();

  const text = String(value ?? '');
  if (/^\d+$/.test(text)) return new Date(Number(text)).toISOString();

  return text;
}

/**
 * What the bot posted and where, so a later approval can find its thread.
 *
 * The bot itself is stateless: a review request is a Slack message and nothing
 * else. Replying to that message when the merge request is approved needs the
 * message `ts` kept somewhere that survives a restart, and the set of approvers
 * already announced so a poll never repeats one.
 */
export class ReviewRequestStore {
  private readonly db: Knex;

  static async create(
    options: ReviewRequestStoreOptions,
  ): Promise<ReviewRequestStore> {
    const store = new ReviewRequestStore(options.database);
    await store.ensureTablesExist();
    return store;
  }

  private constructor(database: Knex) {
    this.db = database;
  }

  private async ensureTablesExist(): Promise<void> {
    if (!(await this.db.schema.hasTable(REQUESTS_TABLE))) {
      await this.db.schema.createTable(REQUESTS_TABLE, table => {
        table.increments('id').primary();
        table.text('url').notNullable();
        table.string('reference').notNullable();
        table.string('channel').notNullable();
        table.string('thread_ts').notNullable();
        table.string('requester').notNullable();
        table.string('status').notNullable().defaultTo('opened');
        table.integer('failure_count').notNullable().defaultTo(0);
        table.timestamp('created_at').notNullable();
        table.timestamp('last_polled_at');
        // The same merge request posted twice in one message is one line, so
        // the poller replies once per thread rather than once per duplicate.
        table.unique(['url', 'channel', 'thread_ts']);
        table.index(['status']);
      });
    }

    if (!(await this.db.schema.hasTable(APPROVALS_TABLE))) {
      await this.db.schema.createTable(APPROVALS_TABLE, table => {
        table
          .integer('request_id')
          .notNullable()
          .references('id')
          .inTable(REQUESTS_TABLE)
          .onDelete('CASCADE');
        table.string('approver').notNullable();
        table.timestamp('notified_at').notNullable();
        table.primary(['request_id', 'approver']);
      });
    }
  }

  /** Records every merge request line of a posted message as a live request. */
  async track(
    entries: { url: string; reference: string }[],
    posted: PostedMessage,
  ): Promise<void> {
    if (entries.length === 0) return;
    const now = new Date();
    await this.db(REQUESTS_TABLE)
      .insert(
        entries.map(entry => ({
          url: entry.url,
          reference: entry.reference,
          channel: posted.channel,
          thread_ts: posted.threadTs,
          requester: posted.requester,
          status: 'opened',
          failure_count: 0,
          created_at: now,
        })),
      )
      .onConflict(['url', 'channel', 'thread_ts'])
      .ignore();
  }

  async listOpen(): Promise<TrackedRequest[]> {
    const rows = await this.db(REQUESTS_TABLE)
      .where({ status: 'opened' })
      .orderBy('id');
    return rows.map(row => this.rowToRequest(row));
  }

  async markStatus(id: number, status: TrackedStatus): Promise<void> {
    await this.db(REQUESTS_TABLE)
      .where({ id })
      .update({ status, last_polled_at: new Date() });
  }

  /** Bumps the consecutive failure count and returns the new value. */
  async recordFailure(id: number): Promise<number> {
    await this.db(REQUESTS_TABLE)
      .where({ id })
      .update({ last_polled_at: new Date() })
      .increment('failure_count', 1);
    const row = await this.db(REQUESTS_TABLE)
      .where({ id })
      .first('failure_count');
    return Number(row?.failure_count ?? 0);
  }

  async recordPolled(id: number): Promise<void> {
    await this.db(REQUESTS_TABLE)
      .where({ id })
      .update({ failure_count: 0, last_polled_at: new Date() });
  }

  /** Usernames already announced in this request's thread. */
  async notifiedApprovers(id: number): Promise<Set<string>> {
    const rows = await this.db(APPROVALS_TABLE)
      .where({ request_id: id })
      .select('approver');
    return new Set(rows.map(row => row.approver as string));
  }

  async recordApprovers(id: number, usernames: string[]): Promise<void> {
    if (usernames.length === 0) return;
    const now = new Date();
    await this.db(APPROVALS_TABLE)
      .insert(
        usernames.map(approver => ({
          request_id: id,
          approver,
          notified_at: now,
        })),
      )
      .onConflict(['request_id', 'approver'])
      .ignore();
  }

  /** Stops polling requests older than the cutoff; returns how many. */
  async expireOlderThan(cutoff: Date): Promise<number> {
    return this.db(REQUESTS_TABLE)
      .where({ status: 'opened' })
      .andWhere('created_at', '<', cutoff)
      .update({ status: 'expired', last_polled_at: new Date() });
  }

  private rowToRequest(row: Record<string, unknown>): TrackedRequest {
    return {
      id: Number(row.id),
      url: row.url as string,
      reference: row.reference as string,
      channel: row.channel as string,
      threadTs: row.thread_ts as string,
      requester: row.requester as string,
      status: row.status as TrackedStatus,
      failureCount: Number(row.failure_count ?? 0),
      createdAt: toIsoStamp(row.created_at),
    };
  }
}
