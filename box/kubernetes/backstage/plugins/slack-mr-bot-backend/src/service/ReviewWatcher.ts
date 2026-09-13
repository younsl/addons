import { LoggerService } from '@backstage/backend-plugin-api';
import { ProviderRouter } from './providers/ProviderRouter';
import { ReviewParticipant, ReviewState } from './providers/types';
import { ReviewRequestStore, TrackedRequest } from './ReviewRequestStore';
import { SlackUserResolver } from './SlackUserResolver';

/** The one Web API method the watcher posts with, stubbed in tests. */
export interface ThreadReplyClient {
  chat: {
    postMessage(args: {
      channel: string;
      thread_ts: string;
      text: string;
    }): Promise<unknown>;
  };
}

export interface ReviewWatcherOptions {
  store: ReviewRequestStore;
  router: ProviderRouter;
  resolver: SlackUserResolver;
  client: ThreadReplyClient;
  logger: LoggerService;
  /** Requests older than this stop being polled. */
  trackDays: number;
  /** Bounds one provider lookup; a stuck GitLab must not stall the tick. */
  lookupTimeoutMs?: number;
}

/** Consecutive lookup failures before a request is given up on. */
const MAX_FAILURES = 5;
const DEFAULT_LOOKUP_TIMEOUT_MS = 10000;

/** Slack errors meaning the thread is gone, so retrying cannot help. */
const GONE_ERRORS = [
  'message_not_found',
  'thread_not_found',
  'channel_not_found',
  'is_archived',
];

/**
 * Follows posted review requests and replies in their thread as reviewers
 * approve and someone merges.
 *
 * Polling rather than a GitLab webhook keeps the plugin's stance that nothing
 * calls into this instance, and needs no per-project hook: the token that
 * looked the merge request up can also read who approved it. Each tick diffs
 * the current approvers against those already announced, so a restart or an
 * overdue run replays nothing.
 */
export class ReviewWatcher {
  constructor(private readonly options: ReviewWatcherOptions) {}

  async tick(): Promise<void> {
    const { store, logger, trackDays } = this.options;

    const cutoff = new Date(Date.now() - trackDays * 24 * 60 * 60 * 1000);
    const expired = await store.expireOlderThan(cutoff);
    if (expired > 0) {
      logger.info(
        `[slack-mr-bot] stopped following ${expired} request(s) older than ${trackDays} days`,
      );
    }

    const open = await store.listOpen();
    if (open.length === 0) return;

    // One lookup per merge request, however many threads carry it.
    const byUrl = new Map<string, TrackedRequest[]>();
    for (const request of open) {
      byUrl.set(request.url, [...(byUrl.get(request.url) ?? []), request]);
    }

    for (const [url, requests] of byUrl) {
      let state: ReviewState;
      try {
        state = await this.fetchState(url);
      } catch (error) {
        await this.noteFailure(requests, error);
        continue;
      }
      for (const request of requests) {
        await this.apply(request, state);
      }
    }
  }

  private async fetchState(url: string): Promise<ReviewState> {
    const controller = new AbortController();
    const timer = setTimeout(
      () => controller.abort(),
      this.options.lookupTimeoutMs ?? DEFAULT_LOOKUP_TIMEOUT_MS,
    );
    try {
      return await this.options.router.fetchState(url, controller.signal);
    } finally {
      clearTimeout(timer);
    }
  }

  private async noteFailure(
    requests: TrackedRequest[],
    error: unknown,
  ): Promise<void> {
    const { store, logger } = this.options;
    for (const request of requests) {
      const count = await store.recordFailure(request.id);
      if (count >= MAX_FAILURES) {
        logger.warn(
          `[slack-mr-bot] giving up on ${request.reference} after ${count} failed lookups: ${error}`,
        );
        await store.markStatus(request.id, 'unreachable');
      }
    }
  }

  private async apply(
    request: TrackedRequest,
    state: ReviewState,
  ): Promise<void> {
    const { store, resolver } = this.options;

    const announced = await store.notifiedApprovers(request.id);
    const fresh = state.approvers.filter(a => !announced.has(a.username));

    const link = `<${request.url}|${request.reference}>`;
    const lines: string[] = [];
    for (const approver of fresh) {
      const who = await resolver.mention(approver, request.requester);
      lines.push(`${who} 님이 ${link} 리뷰를 완료했습니다.`);
    }
    if (state.status === 'merged') {
      lines.push(
        await this.mergedLine(link, state.mergedBy, request.requester),
      );
    }

    if (lines.length > 0) {
      const delivered = await this.reply(request, lines.join('\n'));
      if (!delivered) return;
    }

    // Recorded only after the reply landed: a failed post retries next tick,
    // and the primary key on (request, approver) keeps a retry from doubling up.
    await store.recordApprovers(request.id, fresh.map(a => a.username));

    if (state.status === 'opened') {
      await store.recordPolled(request.id);
    } else {
      await store.markStatus(request.id, state.status);
    }
  }

  private async mergedLine(
    link: string,
    mergedBy: ReviewParticipant | undefined,
    requester: string,
  ): Promise<string> {
    if (!mergedBy) return `${link} 이 머지되었습니다.`;
    const who = await this.options.resolver.mention(mergedBy, requester);
    return `${who} 님이 ${link} 을 머지했습니다.`;
  }

  /** True when posted. A vanished thread ends tracking; other errors retry. */
  private async reply(request: TrackedRequest, text: string): Promise<boolean> {
    const { client, store, logger } = this.options;
    try {
      await client.chat.postMessage({
        channel: request.channel,
        thread_ts: request.threadTs,
        text,
      });
      return true;
    } catch (error) {
      const message = String(error);
      if (GONE_ERRORS.some(code => message.includes(code))) {
        logger.warn(
          `[slack-mr-bot] thread for ${request.reference} is gone, stopping: ${message}`,
        );
        await store.markStatus(request.id, 'unreachable');
      } else {
        logger.warn(
          `[slack-mr-bot] thread reply for ${request.reference} failed, will retry: ${message}`,
        );
      }
      return false;
    }
  }
}
