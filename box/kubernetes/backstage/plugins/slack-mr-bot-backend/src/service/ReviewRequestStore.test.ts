import knex, { Knex } from 'knex';
import { ReviewRequestStore } from './ReviewRequestStore';

const POSTED = { channel: 'C1', threadTs: '1700000000.000100', requester: 'U1' };
const MR_A = { url: 'https://gitlab.example.com/g/p/-/merge_requests/1', reference: '!1' };
const MR_B = { url: 'https://gitlab.example.com/g/p/-/merge_requests/2', reference: '!2' };

describe('ReviewRequestStore', () => {
  let db: Knex;
  let store: ReviewRequestStore;

  beforeEach(async () => {
    db = knex({
      client: 'better-sqlite3',
      connection: { filename: ':memory:' },
      useNullAsDefault: true,
    });
    store = await ReviewRequestStore.create({ database: db });
  });

  afterEach(async () => {
    await db.destroy();
  });

  it('tracks one row per merge request line of a posted message', async () => {
    await store.track([MR_A, MR_B], POSTED);

    const open = await store.listOpen();
    expect(open.map(r => r.reference)).toEqual(['!1', '!2']);
    expect(open[0]).toMatchObject({
      url: MR_A.url,
      channel: 'C1',
      threadTs: POSTED.threadTs,
      requester: 'U1',
      status: 'opened',
      failureCount: 0,
    });
    expect(open[0].createdAt).toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });

  it('keeps the same merge request in two threads as two rows', async () => {
    await store.track([MR_A], POSTED);
    await store.track([MR_A], { ...POSTED, threadTs: '1700000001.000200' });

    expect(await store.listOpen()).toHaveLength(2);
  });

  it('ignores a duplicate of the same line in the same thread', async () => {
    await store.track([MR_A], POSTED);
    await store.track([MR_A], POSTED);

    expect(await store.listOpen()).toHaveLength(1);
  });

  it('drops a request from the open list once its status changes', async () => {
    await store.track([MR_A, MR_B], POSTED);
    const [a] = await store.listOpen();

    await store.markStatus(a.id, 'merged');

    expect((await store.listOpen()).map(r => r.reference)).toEqual(['!2']);
  });

  it('counts consecutive failures and resets them on a good poll', async () => {
    await store.track([MR_A], POSTED);
    const [a] = await store.listOpen();

    expect(await store.recordFailure(a.id)).toBe(1);
    expect(await store.recordFailure(a.id)).toBe(2);
    await store.recordPolled(a.id);

    expect((await store.listOpen())[0].failureCount).toBe(0);
  });

  it('remembers announced approvers per request, ignoring repeats', async () => {
    await store.track([MR_A, MR_B], POSTED);
    const [a, b] = await store.listOpen();

    await store.recordApprovers(a.id, ['alice', 'bob']);
    await store.recordApprovers(a.id, ['bob']);

    expect(await store.notifiedApprovers(a.id)).toEqual(new Set(['alice', 'bob']));
    expect(await store.notifiedApprovers(b.id)).toEqual(new Set());
  });

  it('expires open requests older than the cutoff and leaves newer ones', async () => {
    await store.track([MR_A], POSTED);
    const [a] = await store.listOpen();
    await db('slack_mr_bot_requests')
      .where({ id: a.id })
      .update({ created_at: new Date('2026-01-01T00:00:00Z') });
    await store.track([MR_B], POSTED);

    const expired = await store.expireOlderThan(new Date('2026-06-01T00:00:00Z'));

    expect(expired).toBe(1);
    expect((await store.listOpen()).map(r => r.reference)).toEqual(['!2']);
  });
});
