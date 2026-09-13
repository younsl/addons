import type { PermissionCreate } from "@/services/v1/openapi-types";
import type { RoleAction } from "@/lib/role-actions";

export type RoleActions = PermissionCreate["actions"];

// Every role starts at read. It is the grant that is almost always wanted and
// the only one that cannot damage anything.
export const DEFAULT_ROLE_ACTIONS: RoleActions = ["read"];

export function toggleRoleAction(
  actions: RoleActions,
  action: RoleAction,
): RoleActions {
  return actions.includes(action)
    ? actions.filter((current) => current !== action)
    : [...actions, action];
}

export function canAddRolePermission({
  actions,
  pattern,
}: {
  actions: RoleActions;
  pattern: string;
}): boolean {
  return Boolean(pattern.trim() && actions.length > 0);
}

export function appendRolePermission({
  actions,
  pattern,
  permissions,
}: {
  actions: RoleActions;
  pattern: string;
  permissions: PermissionCreate[];
}): PermissionCreate[] {
  if (!canAddRolePermission({ actions, pattern })) return permissions;

  return [...permissions, { repo_pattern: pattern.trim(), actions: [...actions] }];
}

export function removeRolePermissionAt(
  permissions: PermissionCreate[],
  permissionIndex: number,
): PermissionCreate[] {
  return permissions.filter((_, index) => index !== permissionIndex);
}

export function formatRolePermission(permission: {
  repo_pattern: string;
  actions: string[];
}): string {
  return `${permission.repo_pattern}: ${permission.actions.join(",")}`;
}
