// The accounts every browser test signs in as, and the role each one holds.
//
// The UI gates heavily on these - canViewAccessManagement, canReviewApprovals,
// canEditRepositorySecurity, and a bare me.admin - so testing only as an
// administrator would exercise about half of what a user can see. Each account
// exists to reach a different side of those gates.
//
// The password is the same for all of them and is in the repository on purpose:
// these accounts only ever exist in a throwaway database that `make e2e`
// deletes before each run.
export const E2E_PASSWORD = "e2e-only-not-a-secret";

// Matches FORKLIFT_BOOTSTRAP_ADMIN_USER in playwright.config.ts. The server
// creates it on an empty database and grants it the administrator role.
export const ADMIN_USERNAME = "e2e-admin";

export type E2ERole = "admin" | "auditor" | "approver" | "security" | "reader" | "plain";

export type E2EAccount = {
  role: E2ERole;
  username: string;
  // The role to create and assign, or null to leave the account with none.
  // "*" as the pattern because these test what the UI shows, not what the
  // policy engine matches.
  grants: string[] | null;
};

export const ACCOUNTS: E2EAccount[] = [
  // Created by the server's bootstrap, not by the setup - listed here so tests
  // can name it the same way as the others.
  { role: "admin", username: ADMIN_USERNAME, grants: null },
  // Reads the administrative surfaces but changes nothing.
  { role: "auditor", username: "e2e-auditor", grants: ["audit"] },
  // Decides package approvals. Notably not an admin, so the approval queue has
  // to work without the user and repository listings it links to.
  { role: "approver", username: "e2e-approver", grants: ["approve"] },
  // Edits a repository's security policy without owning the repository.
  { role: "security", username: "e2e-security", grants: ["security"] },
  // Reads repositories and nothing else - the developer who only pulls
  // packages. Distinct from "plain" in the one way that matters for the UI: it
  // can open a repository, so it exercises what a non-administrator sees on a
  // page they are allowed to reach at all.
  { role: "reader", username: "e2e-reader", grants: ["read"] },
  // Signed in with no role whatsoever. Every administrative route must turn it
  // away, and the repository directory comes back empty - a role-less account
  // can read nothing, which is itself worth pinning.
  { role: "plain", username: "e2e-plain", grants: null },
];

export function accountFor(role: E2ERole): E2EAccount {
  const account = ACCOUNTS.find((candidate) => candidate.role === role);
  if (!account) throw new Error(`no e2e account for role ${role}`);

  return account;
}

// Where global setup leaves each account's signed-in cookies. Tests point
// test.use({ storageState }) at these rather than signing in themselves.
export function storageStatePath(role: E2ERole): string {
  return `test/e2e/.auth/${role}.json`;
}
