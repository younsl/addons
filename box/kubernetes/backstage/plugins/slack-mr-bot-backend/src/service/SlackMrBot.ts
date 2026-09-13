import { App, LogLevel } from '@slack/bolt';
import { Config } from '@backstage/config';
import { LoggerService } from '@backstage/backend-plugin-api';
import { GitLabProvider, GitLabHostConfig } from './providers/GitLabProvider';
import { ProviderRouter, ResolvedRequest } from './providers/ProviderRouter';
import { ReviewProvider } from './providers/types';
import { MAX_REQUESTS, parseUrls } from './providers/parseUrls';
import { ReviewRequestStore } from './ReviewRequestStore';

const URLS_CALLBACK_ID = 'slack_mr_bot_urls';
const REVIEW_CALLBACK_ID = 'slack_mr_bot_review';
const URL_BLOCK_PREFIX = 'url_';
const INPUT_ACTION_ID = 'value';
const DEFAULT_HEADER = 'MR 리뷰 부탁드립니다~';
const DEFAULT_COMMAND = '/mr';
/** Lookups run after the modal is open, so this only bounds a stuck request. */
const LOOKUP_TIMEOUT_MS = 10000;

/** Compact keys: private_metadata is capped at 3000 characters. */
interface MetadataEntry {
  u: string;
  r: string;
  a?: number;
  d?: number;
}

interface ReviewMetadata {
  channel: string;
  user: string;
  entries: MetadataEntry[];
}

export interface SlackMrBotOptions {
  config: Config;
  logger: LoggerService;
  /** When given, every posted request is recorded so approvals can follow it. */
  store?: ReviewRequestStore;
}

export class SlackMrBot {
  private constructor(
    private readonly app: App,
    private readonly logger: LoggerService,
    /** Shared with the watcher so both read the same hosts and tokens. */
    readonly router: ProviderRouter,
    private readonly store?: ReviewRequestStore,
  ) {}

  /** The Web API client behind the Socket Mode app, shared with the watcher. */
  get client() {
    return this.app.client;
  }

  static async create(options: SlackMrBotOptions): Promise<SlackMrBot> {
    const { config, logger, store } = options;
    const root = config.getConfig('slackMrBot');

    const app = new App({
      token: root.getString('botToken'),
      appToken: root.getString('appToken'),
      socketMode: true,
      logLevel: LogLevel.INFO,
    });

    const router = new ProviderRouter(
      SlackMrBot.buildProviders(config, logger),
    );
    const command = root.getOptionalString('command') ?? DEFAULT_COMMAND;
    const header = root.getOptionalString('headerText') ?? DEFAULT_HEADER;

    const bot = new SlackMrBot(app, logger, router, store);
    bot.registerHandlers({ command, header, router });
    return bot;
  }

  /** Builds providers from `slackMrBot.providers`, falling back to `integrations`. */
  private static buildProviders(
    config: Config,
    logger: LoggerService,
  ): ReviewProvider[] {
    const overrides = config.getOptionalConfigArray(
      'slackMrBot.providers.gitlab',
    );
    const sources =
      overrides ?? config.getOptionalConfigArray('integrations.gitlab') ?? [];

    const hosts: GitLabHostConfig[] = [];
    for (const source of sources) {
      const host = source.getString('host');
      const token = source.getOptionalString('token');
      if (!token) {
        logger.warn(
          `[slack-mr-bot] GitLab host ${host} has no token, skipping`,
        );
        continue;
      }
      hosts.push({
        host,
        apiBaseUrl:
          source.getOptionalString('apiBaseUrl') ?? `https://${host}/api/v4`,
        token,
      });
    }

    if (hosts.length === 0) {
      logger.warn('[slack-mr-bot] No GitLab host configured');
      return [];
    }
    return [new GitLabProvider(hosts, logger)];
  }

  private registerHandlers(deps: {
    command: string;
    header: string;
    router: ProviderRouter;
  }) {
    const { command, header, router } = deps;

    // `/mr` with URLs skips straight to the titles; `/mr` alone starts at the
    // URL list, which is the only place a multi-line paste is possible since
    // the slash command box is a single line.
    this.app.command(command, async ({ ack, body, client, respond }) => {
      await ack();

      const urls = parseUrls(body.text ?? '');
      if (urls.length > MAX_REQUESTS) {
        await respond({
          response_type: 'ephemeral',
          text: `한 번에 최대 ${MAX_REQUESTS}개까지 올릴 수 있습니다 (입력 ${urls.length}개).`,
        });
        return;
      }

      const base = { channel: body.channel_id, user: body.user_id };

      if (urls.length === 0) {
        await client.views.open({
          trigger_id: body.trigger_id,
          view: this.urlsView(base),
        });
        return;
      }

      const opened = await client.views.open({
        trigger_id: body.trigger_id,
        view: this.loadingView(),
      });
      const viewId = opened.view?.id;
      if (!viewId) return;

      await client.views.update({
        view_id: viewId,
        view: this.reviewView(base, await this.resolve(router, urls)),
      });
    });

    // Step 1 submit: replace the same view with a loading state, then fill it
    // in once every lookup has settled. `response_action` keeps the view id.
    this.app.view(URLS_CALLBACK_ID, async ({ ack, body, view, client }) => {
      const base = JSON.parse(view.private_metadata) as {
        channel: string;
        user: string;
      };
      // One field per merge request, so a URL never has to be typed after a
      // newline. A field holding several pasted URLs still splits correctly.
      const raw = Array.from(
        { length: MAX_REQUESTS },
        (_, index) =>
          view.state.values[`${URL_BLOCK_PREFIX}${index}`]?.[INPUT_ACTION_ID]
            ?.value ?? '',
      ).join(' ');
      const urls = parseUrls(raw);

      if (urls.length === 0) {
        await ack({
          response_action: 'errors',
          errors: {
            [`${URL_BLOCK_PREFIX}0`]: 'MR URL을 하나 이상 입력해주세요.',
          },
        });
        return;
      }
      if (urls.length > MAX_REQUESTS) {
        await ack({
          response_action: 'errors',
          errors: {
            [`${URL_BLOCK_PREFIX}0`]: `한 번에 최대 ${MAX_REQUESTS}개까지 올릴 수 있습니다 (입력 ${urls.length}개).`,
          },
        });
        return;
      }

      await ack({ response_action: 'update', view: this.loadingView() });

      await client.views.update({
        view_id: body.view.id,
        view: this.reviewView(base, await this.resolve(router, urls)),
      });
    });

    // Step 2 submit: post to the channel the command was run in.
    this.app.view(REVIEW_CALLBACK_ID, async ({ ack, view, client }) => {
      const metadata = JSON.parse(view.private_metadata) as ReviewMetadata;

      const lines: string[] = [];
      const posted: MetadataEntry[] = [];
      metadata.entries.forEach((entry, index) => {
        const content =
          view.state.values[`mr_${index}`]?.[INPUT_ACTION_ID]?.value?.trim();
        if (!content) return;
        posted.push(entry);

        const [title, ...rest] = content.split('\n');
        const stats =
          entry.a === undefined || entry.d === undefined
            ? ''
            : ` (+${entry.a} -${entry.d})`;
        lines.push(`<${entry.u}|${entry.r}>: ${title}${stats}`);
        lines.push(...rest);
      });

      if (lines.length === 0) {
        await ack({
          response_action: 'errors',
          errors: { mr_0: '내용을 최소 하나는 입력해주세요.' },
        });
        return;
      }

      await ack();

      const text = [header, ...lines, `요청자: <@${metadata.user}>`].join('\n');
      let result;
      try {
        result = await client.chat.postMessage({ channel: metadata.channel, text });
      } catch (error) {
        this.logger.warn(
          `[slack-mr-bot] postMessage to ${metadata.channel} failed: ${error}`,
        );
        // The channel may not have the bot invited; tell the requester directly.
        result = await client.chat.postMessage({
          channel: metadata.user,
          text: `메시지 게시에 실패했습니다. 채널에 봇이 초대되어 있는지 확인해주세요.\n\n${text}`,
        });
      }
      await this.track(posted, metadata.user, result);
    });
  }

  /**
   * Remembers where the message landed so approvals can reply under it. The
   * response, not the request, names the channel: a DM fallback posts to the
   * user id and comes back with the conversation id the thread lives in.
   */
  private async track(
    entries: MetadataEntry[],
    requester: string,
    result: { channel?: string; ts?: string },
  ): Promise<void> {
    if (!this.store || !result.channel || !result.ts) return;
    try {
      await this.store.track(
        entries.map(entry => ({ url: entry.u, reference: entry.r })),
        { channel: result.channel, threadTs: result.ts, requester },
      );
    } catch (error) {
      // The request is already posted; losing the follow-up is the lesser harm.
      this.logger.warn(`[slack-mr-bot] failed to record posted request: ${error}`);
    }
  }

  private async resolve(
    router: ProviderRouter,
    urls: string[],
  ): Promise<ResolvedRequest[]> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), LOOKUP_TIMEOUT_MS);
    try {
      return await router.resolveAll(urls, controller.signal);
    } finally {
      clearTimeout(timer);
    }
  }

  private urlsView(base: { channel: string; user: string }): any {
    return {
      type: 'modal',
      callback_id: URLS_CALLBACK_ID,
      private_metadata: JSON.stringify(base),
      title: { type: 'plain_text', text: 'MR 리뷰 요청' },
      submit: { type: 'plain_text', text: '다음' },
      close: { type: 'plain_text', text: '취소' },
      blocks: Array.from({ length: MAX_REQUESTS }, (_, index) => ({
        type: 'input',
        block_id: `${URL_BLOCK_PREFIX}${index}`,
        optional: true,
        label: { type: 'plain_text', text: `MR URL ${index + 1}` },
        ...(index === 0
          ? {
              hint: {
                type: 'plain_text',
                text: `최대 ${MAX_REQUESTS}개까지 올릴 수 있습니다.`,
              },
            }
          : {}),
        element: {
          type: 'plain_text_input',
          action_id: INPUT_ACTION_ID,
          placeholder: {
            type: 'plain_text',
            text: 'https://gitlab.example.com/group/project/-/merge_requests/1',
          },
        },
      })),
    };
  }

  private loadingView(): any {
    return {
      type: 'modal',
      title: { type: 'plain_text', text: 'MR 리뷰 요청' },
      close: { type: 'plain_text', text: '취소' },
      blocks: [
        {
          type: 'section',
          text: { type: 'mrkdwn', text: 'MR 정보를 불러오는 중…' },
        },
      ],
    };
  }

  private reviewView(
    base: { channel: string; user: string },
    resolved: ResolvedRequest[],
  ): any {
    const entries: MetadataEntry[] = resolved.map(
      ({ rawUrl, request }, index) => ({
        u: request?.url ?? rawUrl,
        r: request?.reference ?? `#${index + 1}`,
        a: request?.stats?.additions,
        d: request?.stats?.deletions,
      }),
    );

    const blocks = resolved.flatMap(({ request, error }, index) => {
      const stats = request?.stats
        ? `  +${request.stats.additions} -${request.stats.deletions}`
        : '';
      return [
        {
          type: 'input',
          block_id: `mr_${index}`,
          optional: true,
          label: {
            type: 'plain_text',
            text: `${entries[index].r}${stats}`,
          },
          ...(error
            ? {
                hint: {
                  type: 'plain_text',
                  text: `조회 실패 — 직접 입력해주세요 (${error})`.slice(0, 2000),
                },
              }
            : {}),
          element: {
            type: 'plain_text_input',
            action_id: INPUT_ACTION_ID,
            multiline: true,
            ...(request ? { initial_value: request.title } : {}),
          },
        },
      ];
    });

    return {
      type: 'modal',
      callback_id: REVIEW_CALLBACK_ID,
      private_metadata: JSON.stringify({ ...base, entries }),
      title: { type: 'plain_text', text: 'MR 리뷰 요청' },
      submit: { type: 'plain_text', text: '게시' },
      close: { type: 'plain_text', text: '취소' },
      blocks,
    };
  }

  async start(): Promise<void> {
    await this.app.start();
    this.logger.info('[slack-mr-bot] Socket Mode connection started');
  }

  async stop(): Promise<void> {
    await this.app.stop();
    this.logger.info('[slack-mr-bot] Socket Mode connection stopped');
  }
}
