import { useState } from "react";
import { Plus, X } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { RepositoryPatternCombobox } from "@/components/app/repository-pattern-combobox";
import { useRepositoryPatternOptions } from "@/hooks/repositories/use-repository-pattern-options";
import { useTranslation } from "@/lib/i18n";
import { RoleActionCards } from "@/routes/access/roles/-components/role-action-picker";
import {
  DEFAULT_ROLE_ACTIONS,
  appendRolePermission,
  canAddRolePermission,
  formatRolePermission,
  removeRolePermissionAt,
  toggleRoleAction,
  type RoleActions,
} from "@/routes/access/roles/-utils/role-permissions";

import type { PermissionCreate } from "@/services/v1/openapi-types";

// PermissionBuilder collects permissions before the role exists, so nothing here
// calls the API - the list is handed to the caller and sent with the create.
// That is the whole difference from RolePermissionsPanel, which grants against a
// role that already has an id.
export function PermissionBuilder({
  permissions,
  onChange,
}: {
  permissions: PermissionCreate[];
  onChange: (permissions: PermissionCreate[]) => void;
}) {
  const { t } = useTranslation();
  const [pattern, setPattern] = useState("");
  const [actions, setActions] = useState<RoleActions>([...DEFAULT_ROLE_ACTIONS]);
  const { options: repoOptions, types: repoTypes } = useRepositoryPatternOptions();

  const add = () => {
    onChange(appendRolePermission({ actions, pattern, permissions }));
    setPattern("");
    setActions([...DEFAULT_ROLE_ACTIONS]);
  };

  return (
    <div className="space-y-3 border-t border-border pt-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 className="m-0 text-sm font-semibold">{t("common.permissions")}</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            {t("role.permissions-description")}
          </p>
        </div>
        <Badge className="mt-0.5">{permissions.length}</Badge>
      </div>

      <div className="min-h-10 rounded-lg border border-border bg-muted/20 p-2">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          {permissions.map((permission, index) => (
            // Nothing has an id yet, so the index is part of the key: the same
            // pattern may legitimately be added twice with different actions.
            <Badge key={`${permission.repo_pattern}-${index}`} className="font-mono">
              {formatRolePermission(permission)}
              <Button
                className="-mr-1 ml-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                size="icon-xs"
                variant="ghost"
                type="button"
                title={t("common.remove-permission")}
                onClick={() => onChange(removeRolePermissionAt(permissions, index))}
              >
                <X className="size-3" aria-hidden="true" />
              </Button>
            </Badge>
          ))}
          {permissions.length === 0 && (
            <span className="px-1 text-sm text-muted-foreground">
              {t("common.no-permissions-added")}
            </span>
          )}
        </div>
      </div>

      <div className="rounded-lg border border-border/80 bg-background/40 p-3">
        <FieldGroup className="gap-3">
          <Field>
            <FieldLabel>{t("common.repository-pattern")}</FieldLabel>
            <RepositoryPatternCombobox
              className="w-full"
              options={repoOptions}
              types={repoTypes}
              value={pattern}
              onValueChange={setPattern}
            />
          </Field>

          <Field>
            <FieldLabel>{t("common.actions")}</FieldLabel>
            <RoleActionCards
              selected={actions}
              onToggle={(action) => setActions((current) => toggleRoleAction(current, action))}
            />
          </Field>

          <div className="flex justify-end">
            <Button
              variant="outline"
              type="button"
              onClick={add}
              disabled={!canAddRolePermission({ actions, pattern })}
            >
              <Plus data-icon="inline-start" />
              {t("common.add-permission")}
            </Button>
          </div>
        </FieldGroup>
      </div>
    </div>
  );
}
