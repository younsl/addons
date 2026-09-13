import { useNavigate, useParams } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/app-ui/page";
import { QuotasPanel } from "@/components/app-ui/token-quota";
import { useTranslation } from "@/lib/i18n";
import { UserDangerPanel } from "@/routes/access/users/-components/user-danger-panel";
import { UserLockoutPanel } from "@/routes/access/users/-components/user-lockout-panel";
import { UserPasswordPanel } from "@/routes/access/users/-components/user-password-panel";
import { UserRolesPanel } from "@/routes/access/users/-components/user-roles-panel";
import { UserStatusPanel } from "@/routes/access/users/-components/user-status-panel";
import { UserSummaryPanel } from "@/routes/access/users/-components/user-summary-panel";
import { UserTokensPanel } from "@/routes/access/users/-components/user-tokens-panel";
import { useUserDetail } from "@/routes/access/users/-hooks/use-user-detail";

import type { Me } from "@/services/v1/openapi-types";

// Per-user modify page: role mapping, tokens, password reset, enable/disable,
// and the danger zone (impersonate, delete). The Users list is read-only; all
// edits happen here.
export function UserDetailPage({ me }: { me: Me }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams({ strict: false }) as { id?: string };
  const userId = Number(id);
  const { error, isLoading, roles, tokens, user, runAction, setError } = useUserDetail(userId);

  if (isLoading) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (!user) return <Alert className="my-2.5">{error || "User not found."}</Alert>;

  const isSelf = user.username === me.username;
  // Password, lockout: only meaningful for a local account that can sign in.
  // An OIDC user's credentials live in the provider; a robot has none.
  const hasLocalCredentials = me.admin && user.source === "local" && !user.robot;

  return (
    <div data-testid="page-user-detail">
      <PageHeader
        title={
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <span className="min-w-0 truncate">{user.username}</span>
            {user.robot && <Badge variant="warning">{t("user.type-robot")}</Badge>}
            {isSelf && <Badge>{t("common.you")}</Badge>}
          </div>
        }
        actions={
          <Button variant="outline" onClick={() => navigate({ to: "/access/users" })}>
            {t("user.back")}
          </Button>
        }
      />
      {error && <Alert className="mb-4">{error}</Alert>}

      <UserSummaryPanel user={user} />
      <UserRolesPanel user={user} roles={roles} canWrite={Boolean(me.admin)} runAction={runAction} />
      <QuotasPanel tokenUsed={tokens.length} loginFailures={user.failed_login_count ?? 0} robot={user.robot} />
      <UserTokensPanel
        user={user}
        tokens={tokens}
        canWrite={Boolean(me.admin)}
        runAction={runAction}
      />
      {hasLocalCredentials && <UserPasswordPanel user={user} onError={setError} />}
      {hasLocalCredentials && <UserLockoutPanel user={user} runAction={runAction} />}
      {me.admin && <UserStatusPanel user={user} isSelf={isSelf} runAction={runAction} />}
      {me.admin && (
        <UserDangerPanel user={user} isSelf={isSelf} me={me} runAction={runAction} />
      )}
    </div>
  );
}
