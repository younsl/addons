import { useState } from "react";

import { useRepositoryPatternOptions } from "@/hooks/repositories/use-repository-pattern-options";
import { useAddRolePermissionMutation } from "@/routes/access/roles/-hooks/use-role-mutations";
import {
  DEFAULT_ROLE_ACTIONS,
  canAddRolePermission,
  toggleRoleAction,
  type RoleActions,
} from "@/routes/access/roles/-utils/role-permissions";

import type { RoleAction } from "@/lib/role-actions";

// The add-a-permission row on the role detail page: pattern, actions, and the
// call that grants them. Errors go to the caller's runAction, which owns the
// page's single alert.
export function useRolePermissionForm({
  roleId,
  runAction,
}: {
  roleId: number;
  runAction: (action: Promise<unknown>) => void;
}) {
  const [pattern, setPattern] = useState("");
  const [actions, setActions] = useState<RoleActions>([...DEFAULT_ROLE_ACTIONS]);
  const { options: repoOptions, types: repoTypes } = useRepositoryPatternOptions();
  const addPermissionMutation = useAddRolePermissionMutation();

  return {
    actions,
    canAdd: canAddRolePermission({ actions, pattern }),
    isAdding: addPermissionMutation.isPending,
    pattern,
    repoOptions,
    repoTypes,
    addPermission: () => {
      runAction(
        addPermissionMutation.mutateAsync({
          roleId,
          permission: { repo_pattern: pattern.trim(), actions },
        }),
      );
      // Cleared straight away rather than on success: the row is an entry field,
      // and leaving the text sitting there invites a second identical grant.
      setPattern("");
      setActions([...DEFAULT_ROLE_ACTIONS]);
    },
    setPattern,
    toggleAction: (action: RoleAction) =>
      setActions((current) => toggleRoleAction(current, action)),
  };
}
