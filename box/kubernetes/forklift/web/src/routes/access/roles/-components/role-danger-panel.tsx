import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { useTranslation } from "@/lib/i18n";
import { useDeleteRoleMutation } from "@/routes/access/roles/-hooks/use-role-mutations";

import type { Role } from "@/services/v1/openapi-types";

export function RoleDangerPanel({
  role,
  runAction,
}: {
  role: Role;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [isConfirming, setIsConfirming] = useState(false);
  const deleteRoleMutation = useDeleteRoleMutation();

  const remove = () => {
    runAction(
      deleteRoleMutation
        .mutateAsync(role.id)
        // Only on success: a failed delete must leave the user on the page to
        // read why, not drop them back into the directory with no explanation.
        .then(() => navigate({ to: "/access/roles" })),
    );
  };

  return (
    <Card size="sm" className="mb-4 ring-destructive" data-testid="panel-danger-zone">
      <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold text-destructive">
          {t("common.danger-zone")}
        </h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {t("role.delete-confirm")}
        </p>
        <Button
          variant="destructive"
          type="button"
          disabled={deleteRoleMutation.isPending}
          onClick={() => setIsConfirming(true)}
        >
          {t("role.delete")}
        </Button>
        <ConfirmModal
          open={isConfirming}
          title={`Delete role "${role.name}"?`}
          message="Users and group mappings holding this role lose its permissions immediately."
          confirmLabel={t("common.delete")}
          danger
          onConfirm={() => { setIsConfirming(false); remove(); }}
          onCancel={() => setIsConfirming(false)}
        />
      </CardContent>
    </Card>
  );
}
