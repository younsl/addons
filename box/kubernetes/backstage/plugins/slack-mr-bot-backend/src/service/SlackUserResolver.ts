import { LoggerService } from '@backstage/backend-plugin-api';
import { ReviewParticipant } from './providers/types';

/** The two Web API methods used, so tests can pass a stub instead of a client. */
export interface SlackUserClient {
  users: {
    lookupByEmail(args: { email: string }): Promise<{
      user?: { id?: string };
    }>;
    info(args: { user: string }): Promise<{
      user?: { profile?: { email?: string } };
    }>;
  };
}

export interface SlackUserResolverOptions {
  client: SlackUserClient;
  logger: LoggerService;
}

/**
 * Turns a GitLab user into a Slack mention, falling back to the display name.
 *
 * GitLab hands out a user's email only to an admin token, so the address has to
 * be guessed: the workspace domain comes from the requester's own Slack profile,
 * and the GitLab username is the local part, which is how accounts are issued
 * here. A public email on the GitLab profile is tried first when present. Both
 * lookups need the `users:read.email` scope; without it the resolver notes the
 * missing scope once and every participant falls back to a name linked to
 * their GitLab profile.
 */
export class SlackUserResolver {
  private readonly byEmail = new Map<string, string | null>();
  private readonly domainByRequester = new Map<string, string | null>();
  private disabled = false;

  constructor(private readonly options: SlackUserResolverOptions) {}

  /** `<@U…>` when a Slack account matches, otherwise the linked display name. */
  async mention(
    participant: ReviewParticipant,
    requester: string,
  ): Promise<string> {
    if (this.disabled) return SlackUserResolver.fallback(participant);

    if (participant.email) {
      const id = await this.lookup(participant.email);
      if (id) return `<@${id}>`;
    }

    // Only guess when the profile gave nothing usable; the domain lookup is a
    // Slack call of its own and is skipped when the public email already hit.
    const domain = await this.workspaceDomain(requester);
    if (domain) {
      const id = await this.lookup(`${participant.username}@${domain}`);
      if (id) return `<@${id}>`;
    }
    return SlackUserResolver.fallback(participant);
  }

  /** Display name, linked to the provider profile when the provider gave one. */
  static fallback(participant: ReviewParticipant): string {
    return participant.profileUrl
      ? `<${participant.profileUrl}|${participant.name}>`
      : participant.name;
  }

  private async workspaceDomain(requester: string): Promise<string | null> {
    if (this.domainByRequester.has(requester)) {
      return this.domainByRequester.get(requester) ?? null;
    }
    let domain: string | null = null;
    try {
      const result = await this.options.client.users.info({ user: requester });
      const email = result.user?.profile?.email;
      domain = email?.includes('@') ? email.split('@')[1] : null;
    } catch (error) {
      this.noteFailure('users.info', error);
    }
    this.domainByRequester.set(requester, domain);
    return domain;
  }

  private async lookup(email: string): Promise<string | null> {
    const key = email.toLowerCase();
    if (this.byEmail.has(key)) return this.byEmail.get(key) ?? null;

    let id: string | null = null;
    try {
      const result = await this.options.client.users.lookupByEmail({ email });
      id = result.user?.id ?? null;
    } catch (error) {
      // `users_not_found` is the ordinary miss; anything else is worth a line.
      if (!String(error).includes('users_not_found')) {
        this.noteFailure('users.lookupByEmail', error);
      }
    }
    this.byEmail.set(key, id);
    return id;
  }

  private noteFailure(method: string, error: unknown): void {
    const text = String(error);
    if (text.includes('missing_scope')) {
      if (!this.disabled) {
        this.options.logger.warn(
          `[slack-mr-bot] ${method} lacks users:read.email; approvers will be named, not mentioned`,
        );
      }
      this.disabled = true;
      return;
    }
    this.options.logger.warn(`[slack-mr-bot] ${method} failed: ${text}`);
  }
}
