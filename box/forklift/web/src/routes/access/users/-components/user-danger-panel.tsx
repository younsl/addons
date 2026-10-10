import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { UserCog } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { useTranslation } from "@/lib/i18n";
import { ImpersonateModal } from "@/routes/access/users/-components/impersonate-modal";
import { useDeleteUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { Me, User } from "@/services/v1/openapi-types";

export function UserDangerPanel({
  user,
  isSelf,
  me,
  runAction,
}: {
  user: User;
  isSelf: boolean;
  me: Me;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [isConfirmingDelete, setIsConfirmingDelete] = useState(false);
  const [isImpersonating, setIsImpersonating] = useState(false);
  const deleteUserMutation = useDeleteUserMutation();

  // Why each account is off limits, so the disabled button is never a dead end.
  const impersonateBlockedReason =
    isSelf ? t("user.impersonate-self-note")
    : user.robot ? t("user.impersonate-robot-note")
    : user.disabled ? t("user.impersonate-disabled-note")
    : me.impersonator ? t("user.impersonate-active-note")
    : "";

  const remove = () => {
    runAction(
      deleteUserMutation
        .mutateAsync(user.id)
        // Only on success: a failed delete must leave the admin on the page to
        // read why, not drop them back into the directory with no explanation.
        .then(() => navigate({ to: "/access/users" })),
    );
  };

  return (
    <Card size="sm" className="mb-4 ring-destructive" data-testid="panel-danger-zone">
      <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold text-destructive">
          {t("common.danger-zone")}
        </h2>

        <h3 className="m-0 mb-1 text-sm font-semibold">{t("user.impersonate")}</h3>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {t("user.impersonate-note")}
          {impersonateBlockedReason && ` ${impersonateBlockedReason}`}
        </p>
        <Button
          variant="destructive"
          type="button"
          disabled={Boolean(impersonateBlockedReason)}
          onClick={() => setIsImpersonating(true)}
        >
          <UserCog data-icon="inline-start" />
          {t("user.impersonate")}
        </Button>

        <h3 className="m-0 mb-1 mt-6 text-sm font-semibold">{t("user.delete")}</h3>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {t("user.delete-note")}
          {isSelf && ` ${t("user.delete-self-note")}`}
        </p>
        <Button
          variant="destructive"
          type="button"
          disabled={isSelf || deleteUserMutation.isPending}
          onClick={() => setIsConfirmingDelete(true)}
        >
          {t("user.delete")}
        </Button>
        <ConfirmModal
          open={isConfirmingDelete}
          title={`Delete user "${user.username}"?`}
          message="This revokes all of the user's tokens and role assignments. This cannot be undone."
          confirmLabel={t("common.delete")}
          danger
          onConfirm={() => { setIsConfirmingDelete(false); remove(); }}
          onCancel={() => setIsConfirmingDelete(false)}
        />
        {isImpersonating && (
          <ImpersonateModal user={user} onClose={() => setIsImpersonating(false)} />
        )}
      </CardContent>
    </Card>
  );
}
