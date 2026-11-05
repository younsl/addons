import { useState } from "react";
import { X } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Select } from "@/components/app-ui/select";
import { useTranslation } from "@/lib/i18n";
import {
  useAssignUserRoleMutation,
  useRemoveUserRoleMutation,
} from "@/routes/access/users/-hooks/use-user-mutations";

import type { Role, User } from "@/services/v1/openapi-types";

export function UserRolesPanel({
  user,
  roles,
  canWrite,
  runAction,
}: {
  user: User;
  roles: Role[];
  canWrite: boolean;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t } = useTranslation();
  const [selectedRoleId, setSelectedRoleId] = useState("");
  const assignRoleMutation = useAssignUserRoleMutation();
  const removeRoleMutation = useRemoveUserRoleMutation();
  // Only roles the user does not already hold; offering a held role would give
  // a control whose only outcome is a duplicate.
  const assignable = roles.filter(
    (role) => !user.roles.some((held) => held.id === role.id),
  );

  return (
    <Card size="sm" className="mb-4" data-testid="panel-roles">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("common.roles")}</h2>
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          {user.roles.map((role) => (
            <Badge key={role.id} className="gap-1">
              <span>{role.name}</span>
              {canWrite && (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className="-mr-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                  title={t("user.remove-role")}
                  onClick={() =>
                    runAction(
                      removeRoleMutation.mutateAsync({ userId: user.id, roleId: role.id }),
                    )
                  }
                >
                  <X className="size-3" aria-hidden="true" />
                  <span className="sr-only">{t("user.remove-role")}</span>
                </Button>
              )}
            </Badge>
          ))}
          {user.roles.length === 0 && (
            <span className="text-sm text-muted-foreground">{t("user.no-roles")}</span>
          )}
        </div>
        {canWrite && assignable.length > 0 && (
          <div className="mt-4 flex min-w-0 flex-wrap items-stretch gap-2 max-sm:flex-col">
            <Select
              value={selectedRoleId}
              onChange={setSelectedRoleId}
              placeholder={t("user.add-role-placeholder")}
              options={assignable.map((role) => ({
                value: String(role.id),
                label: role.name,
                description: role.description || undefined,
              }))}
            />
            <Button
              variant="outline"
              type="button"
              disabled={!selectedRoleId || assignRoleMutation.isPending}
              onClick={() => {
                runAction(
                  assignRoleMutation.mutateAsync({
                    userId: user.id,
                    roleId: Number(selectedRoleId),
                  }),
                );
                setSelectedRoleId("");
              }}
            >
              {t("common.add")}
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
