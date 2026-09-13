import knex, { Knex } from 'knex';
import { LoggerService } from '@backstage/backend-plugin-api';
import { ProviderRouter } from './providers/ProviderRouter';
import { ReviewProvider, ReviewState } from './providers/types';
import { ReviewRequestStore } from './ReviewRequestStore';
import { ReviewWatcher } from './ReviewWatcher';
import { SlackUserResolver } from './SlackUserResolver';

const logger = {
  warn: jest.fn(),
  info: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn(),
} as unknown as LoggerService;

const HOST = 'gitlab.example.com';
const MR_A = { url: `https://${HOST}/g/p/-/merge_requests/1`, reference: '!1' };
const MR_B = { url: `https://${HOST}/g/p/-/merge_requests/2`, reference: '!2' };
const POSTED = { channel: 'C1', threadTs: '1700000000.000100', requester: 'UREQ' };

const ALICE = { username: 'alice', name: 'Alice Kim', profileUrl: `https://${HOST}/alice` };
const BOB = { username: 'bob', name: 'Bob Lee', profileUrl: `https://${HOST}/bob` };

/** Provider whose state is set per URL by the test. */
class FakeProvider implements ReviewProvider {
  readonly id = 'fake';
  readonly states = new Map<string, ReviewState | Error>();

  supports(url: URL): boolean {
    return url.host === HOST;
  }
  async fetch(): Promise<never> {
    throw new Error('not used');
  }
  async fetchState(url: URL): Promise<ReviewState> {
    const state = this.states.get(url.href);
    if (!state) throw new Error(`no state for ${url.href}`);
    if (state instanceof Error) throw state;
    return state;
  }
}

/** Resolver that mentions alice and knows nobody else. */
const resolver = new SlackUserResolver({
  logger,
  client: {
    users: {
      info: async () => ({ user: { profile: { email: 'req@example.com' } } }),
      lookupByEmail: async ({ email }) =>
        email === 'alice@example.com' ? { user: { id: 'UALICE' } } : {},
    },
  },
});

describe('ReviewWatcher', () => {
  let db: Knex;
  let store: ReviewRequestStore;
  let provider: FakeProvider;
  let postMessage: jest.Mock;
  let watcher: ReviewWatcher;

  beforeEach(async () => {
    jest.clearAllMocks();
    db = knex({
      client: 'better-sqlite3',
      connection: { filename: ':memory:' },
      useNullAsDefault: true,
    });
    store = await ReviewRequestStore.create({ database: db });
    provider = new FakeProvider();
    postMessage = jest.fn(async () => ({ ok: true }));
    watcher = new ReviewWatcher({
      store,
      router: new ProviderRouter([provider]),
      resolver,
      client: { chat: { postMessage } },
      logger,
      trackDays: 14,
    });
  });

  afterEach(async () => {
    await db.destroy();
  });

  it('replies in the thread with one sentence per new approver', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE, BOB] });

    await watcher.tick();

    expect(postMessage).toHaveBeenCalledTimes(1);
    expect(postMessage).toHaveBeenCalledWith({
      channel: 'C1',
      thread_ts: POSTED.threadTs,
      text: [
        `<@UALICE> 님이 <${MR_A.url}|!1> 리뷰를 완료했습니다.`,
        `<https://${HOST}/bob|Bob Lee> 님이 <${MR_A.url}|!1> 리뷰를 완료했습니다.`,
      ].join('\n'),
    });
  });

  it('announces each approver once across ticks', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE] });
    await watcher.tick();

    await watcher.tick();
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE, BOB] });
    await watcher.tick();

    expect(postMessage).toHaveBeenCalledTimes(2);
    expect(postMessage.mock.calls[1][0].text).toBe(
      `<https://${HOST}/bob|Bob Lee> 님이 <${MR_A.url}|!1> 리뷰를 완료했습니다.`,
    );
  });

  it('stays quiet when an approval is withdrawn', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE] });
    await watcher.tick();

    provider.states.set(MR_A.url, { status: 'opened', approvers: [] });
    await watcher.tick();

    expect(postMessage).toHaveBeenCalledTimes(1);
  });

  it('reports the merge and stops following the request', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, {
      status: 'merged',
      approvers: [ALICE],
      mergedBy: BOB,
    });

    await watcher.tick();

    expect(postMessage.mock.calls[0][0].text).toBe(
      [
        `<@UALICE> 님이 <${MR_A.url}|!1> 리뷰를 완료했습니다.`,
        `<https://${HOST}/bob|Bob Lee> 님이 <${MR_A.url}|!1> 을 머지했습니다.`,
      ].join('\n'),
    );
    expect(await store.listOpen()).toEqual([]);
  });

  it('closes a request silently', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'closed', approvers: [] });

    await watcher.tick();

    expect(postMessage).not.toHaveBeenCalled();
    expect(await store.listOpen()).toEqual([]);
  });

  it('looks a merge request up once however many threads carry it', async () => {
    await store.track([MR_A], POSTED);
    await store.track([MR_A], { ...POSTED, channel: 'C2', threadTs: '1700000002.000300' });
    const spy = jest.spyOn(provider, 'fetchState');
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE] });

    await watcher.tick();

    expect(spy).toHaveBeenCalledTimes(1);
    expect(postMessage).toHaveBeenCalledTimes(2);
    expect(postMessage.mock.calls.map(c => c[0].channel).sort()).toEqual(['C1', 'C2']);
  });

  it('keeps an approver unannounced when the reply fails, so it retries', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE] });
    postMessage.mockRejectedValueOnce(new Error('An API error occurred: ratelimited'));

    await watcher.tick();
    await watcher.tick();

    expect(postMessage).toHaveBeenCalledTimes(2);
    expect(await store.notifiedApprovers((await store.listOpen())[0].id)).toEqual(
      new Set(['alice']),
    );
  });

  it('stops following a request whose thread is gone', async () => {
    await store.track([MR_A], POSTED);
    provider.states.set(MR_A.url, { status: 'opened', approvers: [ALICE] });
    postMessage.mockRejectedValueOnce(new Error('An API error occurred: message_not_found'));

    await watcher.tick();

    expect(await store.listOpen()).toEqual([]);
  });

  it('gives up on a merge request after five failed lookups', async () => {
    await store.track([MR_A, MR_B], POSTED);
    provider.states.set(MR_A.url, new Error('GitLab API가 404를 반환했습니다'));
    provider.states.set(MR_B.url, { status: 'opened', approvers: [] });

    for (let i = 0; i < 4; i++) await watcher.tick();
    expect((await store.listOpen()).map(r => r.reference)).toEqual(['!1', '!2']);

    await watcher.tick();
    expect((await store.listOpen()).map(r => r.reference)).toEqual(['!2']);
  });
});
