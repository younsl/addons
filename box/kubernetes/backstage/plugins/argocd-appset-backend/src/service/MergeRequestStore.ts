import { Config } from '@backstage/config';
import { LoggerService } from '@backstage/backend-plugin-api';
import { GitLabApiTarget, resolveGitLabApi } from './gitlab';
import { mapWithConcurrency, normalizeRepoUrl } from './ApplicationSetService';
import { MergeRequestSummary } from './types';

/** Parallel diff reads per listing. Bounded to stay a polite API client. */
const DIFF_FETCH_CONCURRENCY = 8;

/** GitLab caps `per_page` at 100. */
const PAGE_SIZE = 100;

/** Open merge requests read per repository, so a busy one cannot fan out unbounded. */
const MAX_LIST_PAGES = 5;

/** Changed files read per merge request. Past this the diff is a bulk change anyway. */
const MAX_DIFF_PAGES = 10;

/**
 * How long one repository's open merge request list is reused. Several cards
 * share a repository, so opening them in turn costs one listing, not one each.
 */
const LIST_TTL_MS = 30_000;

/** An open merge request with every file it changes. */
export interface OpenMergeRequest extends Omit<MergeRequestSummary, 'matchedFiles'> {
  /** Head commit, which is what the changed file list is cached against */
  sha: string;
  files: string[];
}

/** What the listing alone reports, before the diff is read. */
type ListedMergeRequest = Omit<OpenMergeRequest, 'files' | 'additions' | 'deletions'>;

interface DiffSummary {
  files: string[];
  additions: number;
  deletions: number;
}

/** A file is under a path when it is the path or sits below it. `''` is the root. */
export function fileUnderPath(file: string, path: string): boolean {
  return path === '' || file === path || file.startsWith(`${path}/`);
}

/** Changed files falling under any of the paths, in the merge request's order. */
export function matchFiles(files: string[], paths: string[]): string[] {
  return files.filter(file => paths.some(path => fileUnderPath(file, path)));
}

/**
 * Both sides of every change, so a file moved out of or deleted from a
 * directory still counts as touching it.
 */
export function changedFiles(diffs: any[]): string[] {
  const files = new Set<string>();
  for (const diff of diffs) {
    if (typeof diff?.old_path === 'string') files.add(diff.old_path);
    if (typeof diff?.new_path === 'string') files.add(diff.new_path);
  }
  return [...files];
}

/**
 * Added and removed lines across the diffs. A diff GitLab collapsed or judged
 * too large arrives without its text, so the totals are a lower bound.
 */
export function lineStats(diffs: any[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const diff of diffs) {
    if (typeof diff?.diff !== 'string') continue;
    for (const line of diff.diff.split('\n')) {
      if (line.startsWith('+') && !line.startsWith('+++')) additions++;
      else if (line.startsWith('-') && !line.startsWith('---')) deletions++;
    }
  }
  return { additions, deletions };
}

export function mapMergeRequest(mr: any): ListedMergeRequest {
  return {
    iid: Number(mr.iid),
    title: mr.title ?? '',
    authorName: mr.author?.name ?? 'unknown',
    authorUsername: mr.author?.username ?? '',
    createdAt: mr.created_at ?? '',
    webUrl: mr.web_url ?? '',
    sourceBranch: mr.source_branch ?? '',
    targetBranch: mr.target_branch ?? '',
    draft: !!(mr.draft ?? mr.work_in_progress),
    sha: mr.sha ?? '',
  };
}

/**
 * Open merge requests per repository with the files each one changes. GitLab
 * cannot filter merge requests by path, so every open one is listed and its
 * diff read. The diff is cached against the head commit, so only a merge
 * request that was pushed to since the last read costs a request again.
 */
export class MergeRequestStore {
  private readonly config: Config;
  private readonly logger: LoggerService;
  private readonly fetchFn: typeof fetch;
  private readonly lists = new Map<string, { at: number; mergeRequests: OpenMergeRequest[] }>();
  private readonly inFlight = new Map<string, Promise<OpenMergeRequest[]>>();
  private readonly diffs = new Map<string, DiffSummary & { sha: string }>();

  constructor(options: { config: Config; logger: LoggerService; fetchFn?: typeof fetch }) {
    this.config = options.config;
    this.logger = options.logger;
    this.fetchFn = options.fetchFn ?? fetch;
  }

  /** Open merge requests changing a file under any of the paths, newest first. */
  async listForPaths(repoUrl: string, paths: string[]): Promise<MergeRequestSummary[]> {
    if (paths.length === 0) return [];

    const mergeRequests = await this.listOpen(repoUrl);

    return mergeRequests
      .map(({ files, sha: _sha, ...mr }) => ({ ...mr, matchedFiles: matchFiles(files, paths) }))
      .filter(mr => mr.matchedFiles.length > 0)
      .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
  }

  /**
   * Matching merge request count per `namespace/name`, one listing per
   * repository. An ApplicationSet whose repository cannot be listed is left
   * out rather than reported as zero, since zero would claim nothing is open.
   */
  async countByAppSet(
    appSets: { namespace: string; name: string; repoUrl: string; sourcePaths: string[] }[],
  ): Promise<Record<string, number>> {
    const byRepo = new Map<string, typeof appSets>();
    for (const appSet of appSets) {
      if (!appSet.repoUrl) continue;
      const key = normalizeRepoUrl(appSet.repoUrl);
      byRepo.set(key, [...(byRepo.get(key) ?? []), appSet]);
    }

    const counts: Record<string, number> = {};
    await Promise.all(
      [...byRepo.values()].map(async group => {
        let mergeRequests: OpenMergeRequest[];
        try {
          mergeRequests = await this.listOpen(group[0].repoUrl);
        } catch (error) {
          this.logger.debug(`Skipped merge request count for ${group[0].repoUrl}: ${error}`);
          return;
        }
        for (const appSet of group) {
          counts[`${appSet.namespace}/${appSet.name}`] =
            appSet.sourcePaths.length === 0
              ? 0
              : mergeRequests.filter(
                  mr => matchFiles(mr.files, appSet.sourcePaths).length > 0,
                ).length;
        }
      }),
    );

    return counts;
  }

  async listOpen(repoUrl: string, now: number = Date.now()): Promise<OpenMergeRequest[]> {
    const key = normalizeRepoUrl(repoUrl);

    const cached = this.lists.get(key);
    if (cached && now - cached.at < LIST_TTL_MS) return cached.mergeRequests;

    // Concurrent readers of one repository share a single listing.
    const pending = this.inFlight.get(key);
    if (pending) return pending;

    const request = this.fetchOpen(key, repoUrl)
      .then(mergeRequests => {
        this.lists.set(key, { at: now, mergeRequests });
        return mergeRequests;
      })
      .finally(() => this.inFlight.delete(key));
    this.inFlight.set(key, request);

    return request;
  }

  private async fetchOpen(key: string, repoUrl: string): Promise<OpenMergeRequest[]> {
    const api = resolveGitLabApi(this.config, repoUrl);

    const listed = await this.fetchPages(
      api,
      `projects/${api.encodedPath}/merge_requests`,
      { state: 'opened', order_by: 'created_at', sort: 'desc' },
      MAX_LIST_PAGES,
    );
    const mergeRequests = listed.map(mapMergeRequest).filter(mr => Number.isFinite(mr.iid));

    const result: OpenMergeRequest[] = [];
    await mapWithConcurrency(mergeRequests, DIFF_FETCH_CONCURRENCY, async mr => {
      result.push({ ...mr, ...(await this.diffFor(api, key, mr)) });
    });

    // Diffs of merge requests no longer open would otherwise accumulate forever.
    const open = new Set(mergeRequests.map(mr => `${key}|${mr.iid}`));
    for (const cacheKey of this.diffs.keys()) {
      if (cacheKey.startsWith(`${key}|`) && !open.has(cacheKey)) {
        this.diffs.delete(cacheKey);
      }
    }

    return result;
  }

  /**
   * A diff that cannot be read leaves the merge request with no files, so it
   * drops out of every match rather than failing the whole listing.
   */
  private async diffFor(
    api: GitLabApiTarget,
    key: string,
    mr: ListedMergeRequest,
  ): Promise<DiffSummary> {
    const cacheKey = `${key}|${mr.iid}`;
    const cached = this.diffs.get(cacheKey);
    if (cached && mr.sha && cached.sha === mr.sha) {
      return { files: cached.files, additions: cached.additions, deletions: cached.deletions };
    }

    try {
      const diffs = await this.fetchPages(
        api,
        `projects/${api.encodedPath}/merge_requests/${mr.iid}/diffs`,
        {},
        MAX_DIFF_PAGES,
      );
      const summary = { files: changedFiles(diffs), ...lineStats(diffs) };
      this.diffs.set(cacheKey, { sha: mr.sha, ...summary });
      return summary;
    } catch (error) {
      this.logger.warn(`Failed to read changed files of !${mr.iid}: ${error}`);
      return { files: [], additions: 0, deletions: 0 };
    }
  }

  private async fetchPages(
    api: GitLabApiTarget,
    path: string,
    query: Record<string, string>,
    maxPages: number,
  ): Promise<any[]> {
    const items: any[] = [];

    for (let page = 1; page <= maxPages; page++) {
      const response = await this.fetchFn(
        api.url(path, { ...query, per_page: String(PAGE_SIZE), page: String(page) }),
        { headers: { 'PRIVATE-TOKEN': api.token } },
      );
      if (!response.ok) {
        throw new Error(`GitLab API error: ${response.status} ${response.statusText}`);
      }

      const batch: any[] = await response.json();
      items.push(...batch);

      if (batch.length < PAGE_SIZE || response.headers.get('x-next-page') === '') break;
    }

    return items;
  }
}
