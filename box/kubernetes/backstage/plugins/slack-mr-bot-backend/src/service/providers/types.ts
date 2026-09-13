/** Additions and deletions as shown on the merge request's Changes tab. */
export interface DiffStats {
  additions: number;
  deletions: number;
}

/** A review request target: GitLab merge request or GitHub pull request. */
export interface ReviewRequest {
  /** Provider-side number (GitLab iid, GitHub PR number). */
  number: number;
  title: string;
  /** Web URL used for the Slack link. */
  url: string;
  /** Label rendered in Slack, e.g. `!1728` (GitLab) or `#1728` (GitHub). */
  reference: string;
  /** Absent when the provider could not report them. */
  stats?: DiffStats;
}

/** Someone who acted on a review request: approved it, or merged it. */
export interface ReviewParticipant {
  /** Provider-side login, e.g. the GitLab username. */
  username: string;
  /** Display name, used when no Slack account can be matched. */
  name: string;
  /** Only when the provider exposes it; GitLab shows a public email at most. */
  email?: string;
  /** Profile page, so a name that found no Slack account still links somewhere. */
  profileUrl?: string;
}

export type ReviewStatus = 'opened' | 'merged' | 'closed';

/** Where a review request stands right now, as polled from the provider. */
export interface ReviewState {
  status: ReviewStatus;
  /** Everyone currently approving. Withdrawn approvals simply disappear. */
  approvers: ReviewParticipant[];
  /** Present once merged, when the provider knows who did it. */
  mergedBy?: ReviewParticipant;
}

export interface ReviewProvider {
  /** Provider id used in logs and config, e.g. `gitlab`. */
  readonly id: string;
  /** True when this provider owns the given URL. */
  supports(url: URL): boolean;
  /** Fetch the review request. Throws when the URL is not resolvable. */
  fetch(url: URL, signal?: AbortSignal): Promise<ReviewRequest>;
  /** Fetch approval and merge state. Throws when the URL is not resolvable. */
  fetchState(url: URL, signal?: AbortSignal): Promise<ReviewState>;
}
