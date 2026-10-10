import type { MessageKey } from "@/lib/i18n";
import type { PermissionCreate } from "@/services/v1/openapi-types";

// The grantable actions the API accepts (auth.validRoleAction). Pinned to the
// generated type rather than declared `as const`: if the document gains or
// drops an action this stops compiling, instead of leaving the UI offering a
// button that only ever returns 400 - or hiding one the API would honour.
export const ACTIONS = [
  "read", "write", "delete", "approve", "audit", "security", "admin",
] satisfies PermissionCreate["actions"];

export type RoleAction = (typeof ACTIONS)[number];

// One-line description per action, shown under the action name in pickers.
export const ACTION_DESCRIPTION_KEYS = {
  read: "role.action-read",
  write: "role.action-write",
  delete: "role.action-delete",
  approve: "role.action-approve",
  audit: "role.action-audit",
  security: "role.action-security",
  admin: "role.action-admin",
} satisfies Record<RoleAction, MessageKey>;
