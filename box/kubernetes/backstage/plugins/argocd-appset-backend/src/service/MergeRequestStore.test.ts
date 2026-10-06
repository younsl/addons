import { ConfigReader } from '@backstage/config';
import {
  changedFiles,
  fileUnderPath,
  lineStats,
  mapMergeRequest,
  matchFiles,
  MergeRequestStore,
} from './MergeRequestStore';

const REPO = 'https://gitlab.example.com/devops/k8s.git';
const LIST_PATH = '/api/v4/projects/devops%2Fk8s/merge_requests';

const mockLogger = {
  info: jest.fn(),
  warn: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn().mockReturnThis(),
} as any;

const config = new ConfigReader({
  integrations: { gitlab: [{ host: 'gitlab.example.com', token: 'secret' }] },
});

function mr(iid: number, overrides: Record<string, any> = {}) {
  return {
    iid,
    title: `MR ${iid}`,
    author: { name: `Author ${iid}`, username: `author${iid}` },
    created_at: `2026-10-0${iid}T00:00:00Z`,
    web_url: `https://gitlab.example.com/devops/k8s/-/merge_requests/${iid}`,
    source_branch: `feature-${iid}`,
    target_branch: 'main',
    draft: false,
    sha: `sha-${iid}`,
    ...overrides,
  };
}

const json = (body: unknown, headers: Record<string, string> = {}) =>
  ({
    ok: true,
    status: 200,
    statusText: 'OK',
    json: async () => body,
    headers: { get: (name: string) => headers[name.toLowerCase()] ?? null },
  }) as any;

/** Answers the listing with `list` and each diff from `diffs` keyed by iid. */
function gitlab(list: any[], diffs: Record<number, any[] | 'error'>) {
  return jest.fn(async (input: URL | string) => {
    const url = new URL(String(input));
    if (url.pathname === LIST_PATH) return json(list);

    const iid = Number(url.pathname.match(/merge_requests\/(\d+)\/diffs$/)?.[1]);
    const diff = diffs[iid];
    if (diff === 'error') {
      return { ok: false, status: 500, statusText: 'Server Error' } as any;
    }
    return json(diff ?? []);
  });
}

const store = (fetchFn: jest.Mock) =>
  new MergeRequestStore({ config, logger: mockLogger, fetchFn: fetchFn as any });

const diffCalls = (fetchFn: jest.Mock) =>
  fetchFn.mock.calls.filter(([input]) => String(input).includes('/diffs')).length;

describe('fileUnderPath', () => {
  it.each([
    ['shared/redis/values.yaml', 'shared/redis', true],
    ['shared/redis', 'shared/redis', true],
    ['anything/at/all', '', true],
    // A sibling sharing a prefix is a different directory.
    ['shared/redis-cluster/values.yaml', 'shared/redis', false],
    ['shared/values.yaml', 'shared/redis', false],
  ])('%p under %p is %p', (file, path, expected) => {
    expect(fileUnderPath(file, path)).toBe(expected);
  });
});

describe('matchFiles', () => {
  it('keeps files under any path, in order', () => {
    expect(
      matchFiles(['a/x', 'b/y', 'c/z', 'a/w'], ['a', 'c']),
    ).toEqual(['a/x', 'c/z', 'a/w']);
  });

  it('matches nothing without paths', () => {
    expect(matchFiles(['a/x'], [])).toEqual([]);
  });
});

describe('changedFiles', () => {
  // A file moved out of a directory still touches it.
  it('includes both sides of a rename, once each', () => {
    expect(
      changedFiles([
        { old_path: 'a/x', new_path: 'b/x' },
        { old_path: 'c/y', new_path: 'c/y' },
        {},
      ]),
    ).toEqual(['a/x', 'b/x', 'c/y']);
  });
});

describe('lineStats', () => {
  it('counts added and removed lines, not hunk or file headers', () => {
    expect(
      lineStats([
        { diff: '@@ -1,3 +1,4 @@\n-version: 1.0.0\n+version: 1.1.0\n+appVersion: 2\n context\n' },
        { diff: '--- a/x\n+++ b/x\n-gone\n' },
      ]),
    ).toEqual({ additions: 2, deletions: 2 });
  });

  // A collapsed or too-large diff arrives without its text.
  it('skips diffs without text', () => {
    expect(lineStats([{ too_large: true }, { diff: '' }])).toEqual({
      additions: 0,
      deletions: 0,
    });
  });
});

describe('mapMergeRequest', () => {
  it('reads the legacy work_in_progress flag as draft', () => {
    expect(mapMergeRequest(mr(1, { draft: undefined, work_in_progress: true })).draft).toBe(true);
  });

  it('defaults missing fields', () => {
    expect(mapMergeRequest({ iid: 3 })).toMatchObject({
      iid: 3,
      title: '',
      authorName: 'unknown',
      authorUsername: '',
      draft: false,
    });
  });
});

describe('MergeRequestStore', () => {
  beforeEach(() => jest.clearAllMocks());

  it('returns open merge requests touching the paths, newest first', async () => {
    const fetchFn = gitlab([mr(1), mr(2), mr(3)], {
      1: [{ old_path: 'shared/redis/values.yaml', new_path: 'shared/redis/values.yaml' }],
      2: [{ old_path: 'shared/kafka/Chart.yaml', new_path: 'shared/kafka/Chart.yaml' }],
      3: [
        { old_path: 'shared/redis/Chart.yaml', new_path: 'shared/redis/Chart.yaml' },
        { old_path: 'README.md', new_path: 'README.md' },
      ],
    });

    const result = await store(fetchFn).listForPaths(REPO, ['shared/redis']);

    expect(result.map(m => m.iid)).toEqual([3, 1]);
    expect(result[0]).toEqual({
      iid: 3,
      title: 'MR 3',
      authorName: 'Author 3',
      authorUsername: 'author3',
      createdAt: '2026-10-03T00:00:00Z',
      webUrl: 'https://gitlab.example.com/devops/k8s/-/merge_requests/3',
      sourceBranch: 'feature-3',
      targetBranch: 'main',
      draft: false,
      additions: 0,
      deletions: 0,
      matchedFiles: ['shared/redis/Chart.yaml'],
    });
  });

  it('asks GitLab for open merge requests only', async () => {
    const fetchFn = gitlab([], {});

    await store(fetchFn).listOpen(REPO);

    const url = new URL(String(fetchFn.mock.calls[0][0]));
    expect(url.searchParams.get('state')).toBe('opened');
    expect(fetchFn.mock.calls[0][1]).toEqual({ headers: { 'PRIVATE-TOKEN': 'secret' } });
  });

  it('makes no request without paths', async () => {
    const fetchFn = gitlab([mr(1)], {});

    expect(await store(fetchFn).listForPaths(REPO, [])).toEqual([]);
    expect(fetchFn).not.toHaveBeenCalled();
  });

  // One failed diff must not hide every other merge request.
  it('drops a merge request whose diff cannot be read', async () => {
    const fetchFn = gitlab([mr(1), mr(2)], {
      1: 'error',
      2: [{ old_path: 'apps/x', new_path: 'apps/x' }],
    });

    const result = await store(fetchFn).listForPaths(REPO, ['apps']);

    expect(result.map(m => m.iid)).toEqual([2]);
    expect(mockLogger.warn).toHaveBeenCalled();
  });

  it('fails when the listing itself fails', async () => {
    const fetchFn = jest.fn(async () => ({ ok: false, status: 401, statusText: 'Unauthorized' }));

    await expect(store(fetchFn as any).listOpen(REPO)).rejects.toThrow('401');
  });

  it('reuses one listing within the TTL and shares concurrent requests', async () => {
    const fetchFn = gitlab([mr(1)], { 1: [] });
    const s = store(fetchFn);

    await Promise.all([s.listOpen(REPO, 0), s.listOpen(REPO, 0)]);
    await s.listOpen('https://gitlab.example.com/devops/k8s', 10_000);

    expect(fetchFn).toHaveBeenCalledTimes(2);
  });

  it('rereads a diff only when the head commit moved', async () => {
    let list = [mr(1), mr(2)];
    const fetchFn = jest.fn(async (input: URL | string) =>
      new URL(String(input)).pathname === LIST_PATH ? json(list) : json([]),
    );
    const s = store(fetchFn);

    await s.listOpen(REPO, 0);
    expect(diffCalls(fetchFn)).toBe(2);

    list = [mr(1), mr(2, { sha: 'sha-2b' })];
    await s.listOpen(REPO, 60_000);
    expect(diffCalls(fetchFn)).toBe(3);
  });

  it('follows pagination until a short page', async () => {
    const full = Array.from({ length: 100 }, (_, i) => ({ old_path: `a/${i}`, new_path: `a/${i}` }));
    const fetchFn = jest.fn(async (input: URL | string) => {
      const url = new URL(String(input));
      if (url.pathname === LIST_PATH) return json([mr(1)]);
      return url.searchParams.get('page') === '1'
        ? json(full, { 'x-next-page': '2' })
        : json([{ old_path: 'b/last', new_path: 'b/last' }]);
    });

    const [result] = await store(fetchFn).listOpen(REPO);

    expect(result.files).toHaveLength(101);
    expect(diffCalls(fetchFn)).toBe(2);
  });

  describe('countByAppSet', () => {
    const appSet = (name: string, repoUrl: string, sourcePaths: string[]) => ({
      namespace: 'argocd',
      name,
      repoUrl,
      sourcePaths,
    });

    it('counts matching merge requests with one listing per repository', async () => {
      const fetchFn = gitlab([mr(1), mr(2)], {
        1: [{ old_path: 'shared/redis/values.yaml', new_path: 'shared/redis/values.yaml' }],
        2: [
          { old_path: 'shared/redis/Chart.yaml', new_path: 'shared/redis/Chart.yaml' },
          { old_path: 'shared/kafka/Chart.yaml', new_path: 'shared/kafka/Chart.yaml' },
        ],
      });

      const counts = await store(fetchFn).countByAppSet([
        appSet('redis', REPO, ['shared/redis']),
        appSet('kafka', 'https://gitlab.example.com/devops/k8s', ['shared/kafka']),
        appSet('idle', REPO, ['shared/idle']),
        appSet('pathless', REPO, []),
      ]);

      expect(counts).toEqual({
        'argocd/redis': 2,
        'argocd/kafka': 1,
        'argocd/idle': 0,
        'argocd/pathless': 0,
      });
      expect(
        fetchFn.mock.calls.filter(([input]) => new URL(String(input)).pathname === LIST_PATH),
      ).toHaveLength(1);
    });

    // Zero would claim nothing is open when the answer is unknown.
    it('omits ApplicationSets whose repository cannot be listed', async () => {
      const counts = await store(gitlab([], {})).countByAppSet([
        appSet('github', 'https://github.com/org/repo.git', ['apps']),
        appSet('norepo', '', ['apps']),
        appSet('ok', REPO, ['apps']),
      ]);

      expect(counts).toEqual({ 'argocd/ok': 0 });
    });
  });

  it('refuses a repository with no GitLab integration', async () => {
    await expect(
      store(jest.fn()).listOpen('https://github.com/org/repo.git'),
    ).rejects.toThrow(/No GitLab integration/);
  });
});
