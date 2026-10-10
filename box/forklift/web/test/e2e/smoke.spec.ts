import { expect, seededRepositoryId, SEEDED_REPOSITORY, test } from "./setup/fixtures";

import type { E2ERole } from "./setup/accounts";

// Every route, opened as a role that may see it and a role that may not.
//
// This is the cheapest test in the suite and catches the most: a screen that
// throws on render, a permission gate wired to the wrong helper, a redirect
// that sends someone to the page they were just turned away from. Until now
// that check was done by opening 23 URLs by hand.
//
// The gates are read from the route files, not invented here:
//   canViewAccessManagement  admin, auditor
//   canViewApprovalQueue     admin, auditor, approver
//   canViewAdministration    admin
//   me.admin                 admin
type Screen = {
  path: string;
  // The heading that only this screen shows.
  heading: string | RegExp;
  allowed: E2ERole[];
  // Where a role that may not see it ends up. Most go to the repository
  // directory, which every signed-in user may read.
  deniedGoesTo?: string;
};

// Every signed-in role. "plain" holds no grant at all, so screens that need
// one show it an empty state rather than a redirect - it is still allowed in.
const EVERYONE: E2ERole[] = ["admin", "auditor", "approver", "security", "reader", "plain"];
const ACCESS_MANAGEMENT: E2ERole[] = ["admin", "auditor"];
const APPROVAL_QUEUE: E2ERole[] = ["admin", "auditor", "approver"];
const ADMIN_ONLY: E2ERole[] = ["admin"];

const SCREENS: Screen[] = [
  { path: "/workspace/repositories", heading: "Repositories", allowed: EVERYONE },
  { path: "/workspace/repositories/new", heading: "New repository", allowed: ADMIN_ONLY },
  { path: "/workspace/tokens", heading: "Personal access tokens", allowed: EVERYONE },
  { path: "/workspace/tokens/new", heading: "Create token", allowed: EVERYONE },
  { path: "/workspace/approvals", heading: "Approvals", allowed: APPROVAL_QUEUE },
  // Two hops: bulk sends a denied role to the queue, and the queue turns the
  // same role away again. Only the resting place is asserted - the intermediate
  // URL is there for a moment and racing it makes the test flaky.
  { path: "/workspace/approvals/bulk", heading: "Bulk Approval", allowed: APPROVAL_QUEUE },
  { path: "/access/users", heading: "Users", allowed: ACCESS_MANAGEMENT },
  { path: "/access/users/new", heading: "Create local user", allowed: ADMIN_ONLY },
  { path: "/access/roles", heading: "Roles", allowed: ACCESS_MANAGEMENT },
  { path: "/access/roles/new", heading: "Create role", allowed: ADMIN_ONLY },
  { path: "/admin/notifications", heading: "Notifications", allowed: ADMIN_ONLY },
  { path: "/admin/notifications/new", heading: "Add receiver", allowed: ADMIN_ONLY },
  { path: "/admin/storage", heading: "Storage", allowed: ADMIN_ONLY },
  { path: "/admin/ha", heading: "HA Status", allowed: ADMIN_ONLY },
  { path: "/settings", heading: "Settings", allowed: EVERYONE },
];

const DEFAULT_DENIED_DESTINATION = "/workspace/repositories";

for (const screen of SCREENS) {
  const denied = EVERYONE.filter((role) => !screen.allowed.includes(role));

  test(`${screen.path} renders for ${screen.allowed[0]}`, async ({ signedInAs }) => {
    const page = await signedInAs(screen.allowed[0]);

    await page.goto(screen.path);

    await expect(page.getByRole("heading", { name: screen.heading })).toBeVisible();
    // Still here: nothing redirected us away.
    await expect(page).toHaveURL(new RegExp(`${escapeForUrl(screen.path)}/?$`));
  });

  if (denied.length > 0) {
    test(`${screen.path} turns away ${denied[0]}`, async ({ signedInAs }) => {
      const page = await signedInAs(denied[0]);

      await page.goto(screen.path);

      const destination = screen.deniedGoesTo ?? DEFAULT_DENIED_DESTINATION;
      await expect(page).toHaveURL(new RegExp(`${escapeForUrl(destination)}/?$`));
      await expect(page.getByRole("heading", { name: screen.heading })).toBeHidden();
    });
  }
}

// Most tab panels are headings, but the statistics tab's are plain divs, so
// each entry says how to find its own landmark rather than assuming a role.
type Tab = {
  tab: string;
  landmark: { heading: string | RegExp } | { text: string | RegExp };
  allowed: E2ERole[];
};

const TABS: Tab[] = [
  { tab: "artifacts", landmark: { heading: "Artifacts" }, allowed: EVERYONE },
  { tab: "statistics", landmark: { text: /Vulnerability overview/i }, allowed: EVERYONE },
  { tab: "permissions", landmark: { heading: /Roles/ }, allowed: ACCESS_MANAGEMENT },
  { tab: "audit", landmark: { heading: "Audit log" }, allowed: ACCESS_MANAGEMENT },
  { tab: "security", landmark: { text: /Security controls/i }, allowed: ACCESS_MANAGEMENT },
  { tab: "settings", landmark: { heading: /State/ }, allowed: ACCESS_MANAGEMENT },
];

test.describe("repository detail tabs", () => {
  for (const { tab, landmark, allowed } of TABS) {
    test(`${tab} renders for ${allowed[0]}`, async ({ signedInAs, adminApi }) => {
      const page = await signedInAs(allowed[0]);
      const id = await seededRepositoryId(adminApi);

      await page.goto(`/workspace/repositories/${id}/${tab}`);

      await expect(page.getByRole("heading", { name: SEEDED_REPOSITORY })).toBeVisible();
      const target =
        "heading" in landmark
          ? page.getByRole("heading", { name: landmark.heading })
          : page.getByText(landmark.text);
      await expect(target.first()).toBeVisible();
    });
  }

  // A hidden tab reached by URL falls back to Artifacts rather than erroring.
  //
  // As "reader": it may open the repository but is not an administrator, so
  // Settings is not in its tab list. "plain" cannot be used - with no grant at
  // all the repository itself is a 403, so the fallback never runs. The id is
  // resolved as an administrator for the same reason.
  test("an unreachable tab falls back to artifacts", async ({ signedInAs, adminApi }) => {
    const page = await signedInAs("reader");
    const id = await seededRepositoryId(adminApi);

    await page.goto(`/workspace/repositories/${id}/settings`);

    await expect(page).toHaveURL(new RegExp(`/repositories/${id}/artifacts$`));
  });
});

function escapeForUrl(path: string): string {
  return path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
