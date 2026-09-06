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

export interface ReviewProvider {
  /** Provider id used in logs and config, e.g. `gitlab`. */
  readonly id: string;
  /** True when this provider owns the given URL. */
  supports(url: URL): boolean;
  /** Fetch the review request. Throws when the URL is not resolvable. */
  fetch(url: URL, signal?: AbortSignal): Promise<ReviewRequest>;
}
