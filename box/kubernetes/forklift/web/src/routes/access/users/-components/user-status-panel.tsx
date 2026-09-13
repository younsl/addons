import { Card, CardContent } from "@/components/ui/card";
import { LockNote } from "@/components/app-ui/lock-note";
import { StateBadge } from "@/components/app-ui/status-badge";
import { Switch } from "@/components/ui/switch";
import { useTranslation } from "@/lib/i18n";
import { useUpdateUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { User } from "@/services/v1/openapi-types";

export function UserStatusPanel({
  user,
  isSelf,
  runAction,
}: {
  user: User;
  isSelf: boolean;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const updateUserMutation = useUpdateUserMutation();
  // Disabling yourself locks you out mid-session; the protected admin is the
  // one account that must always be able to sign in.
  const isLockedFromEditing = isSelf || user.protected;

  return (
    <Card size="sm" className="mb-4" data-testid="panel-status">
      <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("common.status")}</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          An <strong>{t("common.status.active")}</strong> {t("user.status-desc-1")}{" "}
          <strong>{t("user.status-desc-disabling")}</strong> {t("user.status-desc-2")}
        </p>
        <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
          <Switch
            checked={!user.disabled}
            disabled={isLockedFromEditing}
            onCheckedChange={(isActive) =>
              runAction(
                updateUserMutation.mutateAsync({
                  userId: user.id,
                  body: { disabled: !isActive },
                }),
              )
            }
            aria-label={user.disabled ? t("user.account-disabled") : t("user.account-active")}
          />
          <span>{user.disabled ? t("user.account-disabled") : t("user.account-active")}</span>
        </label>
        {user.protected ? (
          <LockNote title={t("user.status-locked")}>{t("user.protected-note")}</LockNote>
        ) : isSelf && (
          <LockNote title={t("user.status-locked")}>{t("user.self-note")}</LockNote>
        )}
        {user.locked && (
          <p className="mb-0 mt-3">
            <StateBadge state="locked">{t("common.locked")}</StateBadge>
            <span className="ml-2 text-sm text-muted-foreground">{t("user.locked-note")}</span>
          </p>
        )}
      </CardContent>
    </Card>
  );
}
