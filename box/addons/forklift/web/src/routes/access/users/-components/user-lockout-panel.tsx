import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { StateBadge } from "@/components/app-ui/status-badge";
import { Switch } from "@/components/ui/switch";
import { useTranslation } from "@/lib/i18n";
import { useUpdateUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { User } from "@/services/v1/openapi-types";

// UserLockoutPanel toggles failed-password lockout for a local account and
// unlocks it after a lockout. The default admin is protected: the toggle is
// disabled so it can never be locked out of the only guaranteed admin account.
export function UserLockoutPanel({
  user,
  runAction,
}: {
  user: User;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const updateUserMutation = useUpdateUserMutation();

  return (
    <Card size="sm" className="mb-4" data-testid="panel-lockout">
      <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("common.account-lockout")}</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {t("user.lockout-note")} {user.protected && t("user.lockout-protected-note")}
        </p>
        <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
          <Switch
            checked={user.lockout_enabled}
            disabled={user.protected}
            onCheckedChange={(enabled) =>
              runAction(
                updateUserMutation.mutateAsync({
                  userId: user.id,
                  body: { lockout_enabled: enabled },
                }),
              )
            }
            aria-label={user.lockout_enabled ? t("user.lockout-enabled") : t("user.lockout-disabled")}
          />
          <span>{user.lockout_enabled ? t("user.lockout-enabled") : t("user.lockout-disabled")}</span>
        </label>
        {user.locked && (
          <div className="mt-4 flex min-w-0 items-center gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
            <StateBadge state="locked">{t("common.locked")}</StateBadge>
            <Button
              type="button"
              onClick={() =>
                runAction(
                  updateUserMutation.mutateAsync({ userId: user.id, body: { unlock: true } }),
                )
              }
            >
              {t("user.unlock")}
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
