import type { Me } from "@/services/v1/openapi-types";

// Only the permission flags matter here, so a partial principal is enough - the
// callers hold a full Me, but the rules do not depend on the rest of it.
type Principal = Pick<Me, "admin" | "approver" | "auditor" | "security">;

export function canViewAccessManagement(principal: Principal): boolean {
  return Boolean(principal.admin || principal.auditor);
}

export function canManageAccess(principal: Principal): boolean {
  return Boolean(principal.admin);
}

export function canViewApprovalQueue(principal: Principal): boolean {
  return Boolean(principal.admin || principal.approver || principal.auditor);
}

export function canReviewApprovals(principal: Principal): boolean {
  return Boolean(principal.admin || principal.approver);
}

export function canViewAdministration(principal: Principal): boolean {
  return Boolean(principal.admin);
}

export function canViewRepositoryPolicy(principal: Principal): boolean {
  return Boolean(principal.admin || principal.auditor);
}

// Viewing the security policy and rewriting it are different grants: an auditor
// reads it, a security role edits it. Approving is separate again - it decides
// one package against the policy rather than changing the policy.
export function canEditRepositorySecurity(principal: Principal): boolean {
  return Boolean(principal.admin || principal.security);
}
