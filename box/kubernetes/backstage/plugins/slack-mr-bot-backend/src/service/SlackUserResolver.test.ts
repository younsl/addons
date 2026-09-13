import { LoggerService } from '@backstage/backend-plugin-api';
import { SlackUserClient, SlackUserResolver } from './SlackUserResolver';

const logger = {
  warn: jest.fn(),
  info: jest.fn(),
  error: jest.fn(),
  debug: jest.fn(),
  child: jest.fn(),
} as unknown as LoggerService;

const ALICE = {
  username: 'alice',
  name: 'Alice Kim',
  profileUrl: 'https://gitlab.example.com/alice',
};

function clientWith(overrides: Partial<SlackUserClient['users']> = {}): SlackUserClient {
  return {
    users: {
      lookupByEmail: jest.fn(async () => {
        throw new Error('An API error occurred: users_not_found');
      }),
      info: jest.fn(async () => ({
        user: { profile: { email: 'requester@example.com' } },
      })),
      ...overrides,
    },
  };
}

describe('SlackUserResolver', () => {
  beforeEach(() => jest.clearAllMocks());

  it('mentions the account found by the public email first', async () => {
    const client = clientWith({
      lookupByEmail: jest.fn(async ({ email }) =>
        email === 'alice.public@example.com' ? { user: { id: 'UALICE' } } : {},
      ),
    });
    const resolver = new SlackUserResolver({ client, logger });

    const text = await resolver.mention(
      { ...ALICE, email: 'alice.public@example.com' },
      'UREQ',
    );

    expect(text).toBe('<@UALICE>');
    expect(client.users.info).not.toHaveBeenCalled();
  });

  it("guesses username@<requester's domain> when GitLab shows no email", async () => {
    const client = clientWith({
      lookupByEmail: jest.fn(async ({ email }) =>
        email === 'alice@example.com' ? { user: { id: 'UALICE' } } : {},
      ),
    });
    const resolver = new SlackUserResolver({ client, logger });

    expect(await resolver.mention(ALICE, 'UREQ')).toBe('<@UALICE>');
    expect(client.users.info).toHaveBeenCalledWith({ user: 'UREQ' });
  });

  it('falls back to the name linked to the GitLab profile', async () => {
    const resolver = new SlackUserResolver({ client: clientWith(), logger });

    expect(await resolver.mention(ALICE, 'UREQ')).toBe(
      '<https://gitlab.example.com/alice|Alice Kim>',
    );
    expect(logger.warn).not.toHaveBeenCalled();
  });

  it('caches the domain and each email lookup', async () => {
    const client = clientWith();
    const resolver = new SlackUserResolver({ client, logger });

    await resolver.mention(ALICE, 'UREQ');
    await resolver.mention(ALICE, 'UREQ');

    expect(client.users.info).toHaveBeenCalledTimes(1);
    expect(client.users.lookupByEmail).toHaveBeenCalledTimes(1);
  });

  it('stops looking up after a missing_scope error and warns once', async () => {
    const client = clientWith({
      info: jest.fn(async () => {
        throw new Error('An API error occurred: missing_scope');
      }),
    });
    const resolver = new SlackUserResolver({ client, logger });

    await resolver.mention(ALICE, 'UREQ');
    await resolver.mention({ ...ALICE, username: 'bob', name: 'Bob' }, 'UREQ');

    expect(client.users.info).toHaveBeenCalledTimes(1);
    expect(client.users.lookupByEmail).not.toHaveBeenCalled();
    expect(logger.warn).toHaveBeenCalledTimes(1);
  });
});
