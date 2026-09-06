import {
  coreServices,
  createBackendPlugin,
} from '@backstage/backend-plugin-api';
import { SlackMrBot } from './service/SlackMrBot';

export const slackMrBotPlugin = createBackendPlugin({
  pluginId: 'slack-mr-bot',
  register(env) {
    env.registerInit({
      deps: {
        logger: coreServices.logger,
        config: coreServices.rootConfig,
        lifecycle: coreServices.rootLifecycle,
      },
      async init({ logger, config, lifecycle }) {
        const enabled =
          config.getOptionalBoolean('slackMrBot.enabled') ?? true;
        if (!enabled || !config.has('slackMrBot.botToken')) {
          logger.info(
            'Slack MR bot backend plugin is disabled or not configured',
          );
          return;
        }

        logger.info('Initializing Slack MR bot backend plugin');

        const bot = await SlackMrBot.create({ config, logger });
        await bot.start();

        lifecycle.addShutdownHook(async () => {
          await bot.stop();
        });

        logger.info('Slack MR bot backend plugin initialized');
      },
    });
  },
});

export default slackMrBotPlugin;
