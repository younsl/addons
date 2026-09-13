import fetch from 'node-fetch';
import { LoggerService } from '@backstage/backend-plugin-api';
import {
  DiffStats,
  ReviewParticipant,
  ReviewProvider,
  ReviewRequest,
  ReviewState,
  ReviewStatus,
} from './types';

export interface GitLabHostConfig {
  host: string;
  apiBaseUrl: string;
  token: string;
}

interface GitLabRestUser {
  username: string;
  name: string;
  web_url?: string;
  public_email?: string | null;
}

interface GitLabRestMergeRequest {
  iid: number;
  title: string;
  web_url: string;
  state: string;
  merge_user?: GitLabRestUser | null;
  merged_by?: GitLabRestUser | null;
}

interface GitLabRestApprovals {
  approved_by?: { user: GitLabRestUser }[];
}

interface GitLabGraphQlUser {
  username: string;
  name: string;
  webUrl?: string;
  publicEmail?: string | null;
}

interface GitLabGraphQlResponse<T> {
  data?: { project?: { mergeRequest?: T | null } | null };
  errors?: { message: string }[];
}

interface GitLabGraphQlSummary {
  iid: string;
  title: string;
  webUrl: string;
  diffStatsSummary?: { additions: number; deletions: number };
}

interface GitLabGraphQlState {
  state: string;
  approvedBy?: { nodes: GitLabGraphQlUser[] };
  mergeUser?: GitLabGraphQlUser | null;
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

// `approvedBy` is the basic approval every tier has, not the Premium rules
// endpoint, so one query serves Free and Premium alike.
const STATE_QUERY = `
  query($fullPath: ID!, $iid: String!) {
    project(fullPath: $fullPath) {
      mergeRequest(iid: $iid) {
        state
        approvedBy { nodes { username name webUrl publicEmail } }
        mergeUser { username name webUrl publicEmail }
      }
    }
  }
`;

/** GitLab also reports `locked`, a transient state during merge; still open. */
function toStatus(state: string): ReviewStatus {
  switch (state) {
    case 'merged':
      return 'merged';
    case 'closed':
      return 'closed';
    default:
      return 'opened';
  }
}

function participant(
  username: string,
  name: string,
  email?: string | null,
  profileUrl?: string | null,
): ReviewParticipant {
  return {
    username,
    name,
    ...(email ? { email } : {}),
    ...(profileUrl ? { profileUrl } : {}),
  };
}

function fromGraphQl(user: GitLabGraphQlUser): ReviewParticipant {
  return participant(user.username, user.name, user.publicEmail, user.webUrl);
}

function fromRest(user: GitLabRestUser): ReviewParticipant {
  return participant(user.username, user.name, user.public_email, user.web_url);
}

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
    const { host, projectPath, iid } = this.locate(url);

    // GraphQL carries the diff stats the REST merge request object omits, so
    // one request covers title, URL and the +/- counts.
    const mr = await this.graphQl<GitLabGraphQlSummary>(
      host,
      DIFF_STATS_QUERY,
      projectPath,
      iid,
      signal,
    );
    if (mr) {
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
    }

    const rest = await this.rest<GitLabRestMergeRequest>(
      host,
      `${this.mrPath(projectPath, iid)}`,
      signal,
    );
    return {
      number: rest.iid,
      title: rest.title,
      url: rest.web_url,
      reference: `!${rest.iid}`,
    };
  }

  async fetchState(url: URL, signal?: AbortSignal): Promise<ReviewState> {
    const { host, projectPath, iid } = this.locate(url);

    const mr = await this.graphQl<GitLabGraphQlState>(
      host,
      STATE_QUERY,
      projectPath,
      iid,
      signal,
    );
    if (mr) {
      return {
        status: toStatus(mr.state),
        approvers: (mr.approvedBy?.nodes ?? []).map(fromGraphQl),
        ...(mr.mergeUser ? { mergedBy: fromGraphQl(mr.mergeUser) } : {}),
      };
    }

    // REST keeps approvals on a separate endpoint, so the fallback is two calls.
    const base = this.mrPath(projectPath, iid);
    const [rest, approvals] = await Promise.all([
      this.rest<GitLabRestMergeRequest>(host, base, signal),
      this.rest<GitLabRestApprovals>(host, `${base}/approvals`, signal),
    ]);
    const merger = rest.merge_user ?? rest.merged_by;
    return {
      status: toStatus(rest.state),
      approvers: (approvals.approved_by ?? []).map(a => fromRest(a.user)),
      ...(merger ? { mergedBy: fromRest(merger) } : {}),
    };
  }

  private locate(url: URL): {
    host: GitLabHostConfig;
    projectPath: string;
    iid: string;
  } {
    const host = this.hosts.find(h => h.host === url.host);
    const match = url.pathname.match(MR_PATH);
    if (!host || !match) {
      throw new Error(`GitLab MR URL이 아닙니다: ${url.href}`);
    }
    return { host, projectPath: decodeURIComponent(match[1]), iid: match[2] };
  }

  private mrPath(projectPath: string, iid: string): string {
    return `/projects/${encodeURIComponent(projectPath)}/merge_requests/${iid}`;
  }

  /**
   * Runs a merge request query, returning undefined when GraphQL is rejected
   * (an older instance, or a token it does not accept) so the caller can fall
   * back to REST. An abort propagates, since retrying REST would only stall.
   */
  private async graphQl<T>(
    host: GitLabHostConfig,
    query: string,
    projectPath: string,
    iid: string,
    signal?: AbortSignal,
  ): Promise<T | undefined> {
    const endpoint = host.apiBaseUrl.replace(/\/api\/v4\/?$/, '/api/graphql');
    try {
      const response = await fetch(endpoint, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${host.token}`,
        },
        body: JSON.stringify({
          query,
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

      const body = (await response.json()) as GitLabGraphQlResponse<T>;
      const mr = body.data?.project?.mergeRequest;
      if (!mr) {
        if (body.errors?.length) {
          this.logger.warn(
            `[slack-mr-bot] GitLab GraphQL error for ${projectPath}!${iid}: ${body.errors[0].message}`,
          );
        }
        return undefined;
      }
      return mr;
    } catch (error) {
      if (signal?.aborted) throw error;
      this.logger.warn(
        `[slack-mr-bot] GitLab GraphQL request failed for ${projectPath}!${iid}: ${error}`,
      );
      return undefined;
    }
  }

  private async rest<T>(
    host: GitLabHostConfig,
    path: string,
    signal?: AbortSignal,
  ): Promise<T> {
    const endpoint = `${host.apiBaseUrl.replace(/\/$/, '')}${path}`;
    const response = await fetch(endpoint, {
      headers: { 'PRIVATE-TOKEN': host.token },
      signal: signal as any,
    });
    if (!response.ok) {
      this.logger.warn(
        `[slack-mr-bot] GitLab API ${response.status} for ${path}`,
      );
      throw new Error(`GitLab API가 ${response.status}를 반환했습니다 (${path})`);
    }
    return (await response.json()) as T;
  }
}
