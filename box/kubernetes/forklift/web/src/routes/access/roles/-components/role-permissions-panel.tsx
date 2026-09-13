import { X } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { RepositoryPatternCombobox } from "@/components/app/repository-pattern-combobox";
import { useTranslation } from "@/lib/i18n";
import { RoleActionCheckboxes } from "@/routes/access/roles/-components/role-action-picker";
import { useRolePermissionForm } from "@/routes/access/roles/-hooks/use-role-permission-form";
import { useRemoveRolePermissionMutation } from "@/routes/access/roles/-hooks/use-role-mutations";
import { formatRolePermission } from "@/routes/access/roles/-utils/role-permissions";

import type { Role } from "@/services/v1/openapi-types";

export function RolePermissionsPanel({
  role,
  canWrite,
  runAction,
}: {
  role: Role;
  canWrite: boolean;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const form = useRolePermissionForm({ roleId: role.id, runAction });
  const removePermissionMutation = useRemoveRolePermissionMutation();

  return (
    <Card size="sm" className="mb-4" data-testid="panel-permissions">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("common.permissions")}</h2>
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          {role.permissions.map((permission) => (
            <Badge key={permission.id} className="gap-1 font-mono">
              <span>{formatRolePermission(permission)}</span>
              {canWrite && (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className="-mr-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                  title={t("common.remove-permission")}
                  onClick={() =>
                    runAction(
                      removePermissionMutation.mutateAsync({
                        roleId: role.id,
                        permissionId: permission.id,
                      }),
                    )
                  }
                >
                  <X className="size-3" aria-hidden="true" />
                  <span className="sr-only">{t("common.remove-permission")}</span>
                </Button>
              )}
            </Badge>
          ))}
          {role.permissions.length === 0 && (
            <span className="text-sm text-muted-foreground">
              {t("common.no-permissions-granted")}
            </span>
          )}
        </div>
        {canWrite && (
          <div className="mt-4 flex min-w-0 flex-wrap items-stretch gap-2 max-sm:flex-col">
            <RepositoryPatternCombobox
              className="w-full sm:w-[200px]"
              options={form.repoOptions}
              types={form.repoTypes}
              value={form.pattern}
              onValueChange={form.setPattern}
            />
            <RoleActionCheckboxes selected={form.actions} onToggle={form.toggleAction} />
            <Button
              variant="outline"
              type="button"
              disabled={!form.canAdd || form.isAdding}
              onClick={form.addPermission}
            >
              {t("common.add")}
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
