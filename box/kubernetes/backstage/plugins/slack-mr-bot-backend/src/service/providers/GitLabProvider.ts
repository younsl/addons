import fetch from 'node-fetch';
import { LoggerService } from '@backstage/backend-plugin-api';
import { DiffStats, ReviewProvider, ReviewRequest } from './types';

export interface GitLabHostConfig {
  host: string;
  apiBaseUrl: string;
  token: string;
}

interface GitLabRestMergeRequest {
  iid: number;
  title: string;
  web_url: string;
}

interface GitLabGraphQlResponse {
  data?: {
    project?: {
      mergeRequest?: {
        iid: string;
        title: string;
        webUrl: string;
        diffStatsSummary?: { additions: number; deletions: number };
      } | null;
    } | null;
  };
  errors?: { message: string }[];
}

/**
 * Resolves `https://<host>/<namespace>/<project>/-/merge_requests/<iid>` URLs.
 * Namespaces may be nested, so everything before `/-/merge_requests/` is the
 * project path. The tail is anchored so a URL with trailing junk — two URLs
 * pasted without a separator, say — fails loudly instead of resolving to the
 * first number it happens to find.
 */
const MR_PATH = /^\/(.+?)\/-\/merge_requests\/(\d+)\/?$/;

const DIFF_STATS_QUERY = `
  query($fullPath: ID!, $iid: String!) {
    project(fullPath: $fullPath) {
      mergeRequest(iid: $iid) {
        iid
        title
        webUrl
        diffStatsSummary { additions deletions }
      }
    }
  }
`;

export class GitLabProvider implements ReviewProvider {
  readonly id = 'gitlab';

  constructor(
    private readonly hosts: GitLabHostConfig[],
    private readonly logger: LoggerService,
  ) {}

  supports(url: URL): boolean {
    return (
      this.hosts.some(h => h.host === url.host) && MR_PATH.test(url.pathname)
    );
  }

  async fetch(url: URL, signal?: AbortSignal): Promise<ReviewRequest> {
    const host = this.hosts.find(h => h.host === url.host);
    const match = url.pathname.match(MR_PATH);
    if (!host || !match) {
      throw new Error(`GitLab MR URL이 아닙니다: ${url.href}`);
    }

    const projectPath = decodeURIComponent(match[1]);
    const iid = match[2];

    // GraphQL carries the diff stats the REST merge request object omits, so
    // one request covers title, URL and the +/- counts.
    const viaGraphQl = await this.fetchGraphQl(host, projectPath, iid, signal);
    if (viaGraphQl) return viaGraphQl;

    return this.fetchRest(host, projectPath, iid, signal);
  }

  private async fetchGraphQl(
    host: GitLabHostConfig,
    projectPath: string,
    iid: string,
    signal?: AbortSignal,
  ): Promise<ReviewRequest | undefined> {
    const endpoint = host.apiBaseUrl.replace(/\/api\/v4\/?$/, '/api/graphql');
    try {
      const response = await fetch(endpoint, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${host.token}`,
        },
        body: JSON.stringify({
          query: DIFF_STATS_QUERY,
          variables: { fullPath: projectPath, iid },
        }),
        signal: signal as any,
      });
      if (!response.ok) {
        this.logger.warn(
          `[slack-mr-bot] GitLab GraphQL ${response.status} for ${projectPath}!${iid}`,
        );
        return undefined;
      }

      const body = (await response.json()) as GitLabGraphQlResponse;
      const mr = body.data?.project?.mergeRequest;
      if (!mr) {
        if (body.errors?.length) {
          this.logger.warn(
            `[slack-mr-bot] GitLab GraphQL error for ${projectPath}!${iid}: ${body.errors[0].message}`,
          );
        }
        return undefined;
      }

      const summary = mr.diffStatsSummary;
      const stats: DiffStats | undefined = summary
        ? { additions: summary.additions, deletions: summary.deletions }
        : undefined;

      return {
        number: Number(mr.iid),
        title: mr.title,
        url: mr.webUrl,
        reference: `!${mr.iid}`,
        stats,
      };
    } catch (error) {
      if (signal?.aborted) throw error;
      this.logger.warn(
        `[slack-mr-bot] GitLab GraphQL request failed for ${projectPath}!${iid}: ${error}`,
      );
      return undefined;
    }
  }

  /** Fallback without diff stats, for a token or instance GraphQL rejects. */
  private async fetchRest(
    host: GitLabHostConfig,
    projectPath: string,
    iid: string,
    signal?: AbortSignal,
  ): Promise<ReviewRequest> {
    const endpoint = `${host.apiBaseUrl.replace(/\/$/, '')}/projects/${encodeURIComponent(
      projectPath,
    )}/merge_requests/${iid}`;

    const response = await fetch(endpoint, {
      headers: { 'PRIVATE-TOKEN': host.token },
      signal: signal as any,
    });
    if (!response.ok) {
      this.logger.warn(
        `[slack-mr-bot] GitLab API ${response.status} for ${projectPath}!${iid}`,
      );
      throw new Error(
        `GitLab API가 ${response.status}를 반환했습니다 (${projectPath}!${iid})`,
      );
    }

    const mr = (await response.json()) as GitLabRestMergeRequest;
    return {
      number: mr.iid,
      title: mr.title,
      url: mr.web_url,
      reference: `!${mr.iid}`,
    };
  }
}
