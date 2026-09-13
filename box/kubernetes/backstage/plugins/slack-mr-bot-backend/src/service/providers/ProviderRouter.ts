import { ReviewProvider, ReviewRequest, ReviewState } from './types';

/** One entry of a review request batch: resolved, or failed with a reason. */
export interface ResolvedRequest {
  rawUrl: string;
  request?: ReviewRequest;
  error?: string;
}

/** Picks the provider owning a URL and fetches the review request from it. */
export class ProviderRouter {
  constructor(private readonly providers: ReviewProvider[]) {}

  async resolve(rawUrl: string, signal?: AbortSignal): Promise<ReviewRequest> {
    const { provider, url } = this.route(rawUrl);
    return provider.fetch(url, signal);
  }

  /** Same routing as `resolve`, for the poller that follows a posted request. */
  async fetchState(rawUrl: string, signal?: AbortSignal): Promise<ReviewState> {
    const { provider, url } = this.route(rawUrl);
    return provider.fetchState(url, signal);
  }

  private route(rawUrl: string): { provider: ReviewProvider; url: URL } {
    let url: URL;
    try {
      url = new URL(rawUrl);
    } catch {
      throw new Error(`올바른 URL이 아닙니다: ${rawUrl}`);
    }

    const provider = this.providers.find(p => p.supports(url));
    if (!provider) {
      throw new Error(`지원하지 않는 URL입니다: ${url.href}`);
    }
    return { provider, url };
  }

  /**
   * Resolves a batch in parallel, keeping input order. A failed entry does not
   * fail the batch: the modal keeps its row so the author can type the title.
   */
  async resolveAll(
    rawUrls: string[],
    signal?: AbortSignal,
  ): Promise<ResolvedRequest[]> {
    const settled = await Promise.allSettled(
      rawUrls.map(rawUrl => this.resolve(rawUrl, signal)),
    );
    return settled.map((result, index) =>
      result.status === 'fulfilled'
        ? { rawUrl: rawUrls[index], request: result.value }
        : {
            rawUrl: rawUrls[index],
            error:
              result.reason instanceof Error
                ? result.reason.message
                : String(result.reason),
          },
    );
  }
}
