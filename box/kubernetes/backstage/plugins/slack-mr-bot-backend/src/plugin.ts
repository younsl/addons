import {
  coreServices,
  createBackendPlugin,
} from '@backstage/backend-plugin-api';
import { ReviewRequestStore } from './service/ReviewRequestStore';
import { ReviewWatcher } from './service/ReviewWatcher';
import { SlackMrBot } from './service/SlackMrBot';
import { SlackUserResolver } from './service/SlackUserResolver';

const DEFAULT_POLL_SECONDS = 60;
const DEFAULT_TRACK_DAYS = 14;

export const slackMrBotPlugin = createBackendPlugin({
  pluginId: 'slack-mr-bot',
  register(env) {
    env.registerInit({
      deps: {
        logger: coreServices.logger,
        config: coreServices.rootConfig,
        lifecycle: coreServices.rootLifecycle,
        database: coreServices.database,
        scheduler: coreServices.scheduler,
      },
      async init({ logger, config, lifecycle, database, scheduler }) {
        const enabled =
          config.getOptionalBoolean('slackMrBot.enabled') ?? true;
        if (!enabled || !config.has('slackMrBot.botToken')) {
          logger.info(
            'Slack MR bot backend plugin is disabled or not configured',
          );
          return;
        }

        logger.info('Initializing Slack MR bot backend plugin');

        // Every key under reviewNotify has a default, so an existing deployment
        // picks the feature up on upgrade with no chart or config change.
        const notify = config.getOptionalConfig('slackMrBot.reviewNotify');
        const notifyEnabled = notify?.getOptionalBoolean('enabled') ?? true;

        const store = notifyEnabled
          ? await ReviewRequestStore.create({
              database: await database.getClient(),
            })
          : undefined;

        const bot = await SlackMrBot.create({ config, logger, store });
        await bot.start();

        lifecycle.addShutdownHook(async () => {
          await bot.stop();
        });

        if (store) {
          const watcher = new ReviewWatcher({
            store,
            router: bot.router,
            resolver: new SlackUserResolver({ client: bot.client, logger }),
            client: bot.client,
            logger,
            trackDays:
              notify?.getOptionalNumber('trackDays') ?? DEFAULT_TRACK_DAYS,
          });
          const seconds =
            notify?.getOptionalNumber('pollIntervalSeconds') ??
            DEFAULT_POLL_SECONDS;
          await scheduler.scheduleTask({
            id: 'slack-mr-bot-review-poll',
            frequency: { seconds },
            timeout: { minutes: 2 },
            initialDelay: { seconds: 15 },
            fn: async () => {
              try {
                await watcher.tick();
              } catch (error) {
                logger.error(`[slack-mr-bot] review poll failed: ${error}`);
              }
            },
          });
          logger.info(
            `[slack-mr-bot] following review requests every ${seconds}s`,
          );
        }

        logger.info('Slack MR bot backend plugin initialized');
      },
    });
  },
});

export default slackMrBotPlugin;
