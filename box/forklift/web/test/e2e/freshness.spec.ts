import type { APIRequestContext, Page } from "@playwright/test";

import {
  expect,
  scopedName,
  seededRepositoryId,
  test,
  typeRepositoryPattern,
} from "./setup/fixtures";

// What a write on one screen does to another.
//
// This is the class of bug the unit tests cannot reach. They prove a mutation
// asks the cache to refetch; only a browser proves the number on the other
// screen actually changed. Four such bugs were found by auditing the mutation
// hooks by hand - these are the ones a machine can keep finding.
//
// Two things make each of these a real test rather than a coincidence:
//
//   1. The action under test goes through the UI. Arranging state through the
//      API is fine, but a mutation made through the API never touches the
//      browser's cache, so it would prove nothing about invalidation.
//   2. The second screen is visited BEFORE the write, so its query is already
//      cached. The client holds data for 15 seconds, so without invalidation a
//      revisit inside that window serves the stale copy - which is exactly the
//      failure being pinned.

test.describe("a write refreshes the other screen", () => {
  // The queue is keyed by its filters. Before the fix, a decision refreshed
  // only the filter the reviewer was looking at, so the same row stayed listed
  // under its old status everywhere else.
  test("a decision moves the row between status filters", async ({ signedInAs, adminApi }) => {
    const packageName = scopedName("pkg");
    const repo = await proxyRepositoryName(adminApi);

    // Arranged through the API: this is the fixture, not the behaviour.
    const created = await adminApi.post("/api/v1/approvals", {
      data: { repo, package: packageName, status: "approved", note: "e2e fixture" },
    });
    expect(created.ok()).toBe(true);

    const page = await signedInAs("admin");
    await page.goto("/workspace/approvals");

    // Warm the rejected filter first, so a stale cache would be visible later.
    await selectStatus(page, "rejected");
    await expect(page.getByTestId(`row-${packageName}`)).toBeHidden();

    await selectStatus(page, "approved");
    const row = page.getByTestId(`row-${packageName}`);
    await expect(row).toBeVisible();

    // Review in the queue opens the request's own page; the decision is made
    // there, which is what makes this a cross-screen test rather than a local
    // one.
    await row.getByRole("button", { name: "Review" }).click();
    await expect(page).toHaveURL(/\/workspace\/approvals\/\d+$/);
    // Scoped to the detail page, not the document: the queue behind it has a
    // Review button on every row, and for a moment after the navigation both
    // are in the DOM. An unscoped locator matches all of them and fails on
    // strict mode - which reads like a missing button rather than a race.
    const detail = page.getByTestId("page-approval-detail");
    await detail.getByRole("button", { name: "Review" }).click();
    await page.getByTestId("action-reject").click();

    await page.goto("/workspace/approvals");

    // Gone from the filter it was decided under...
    await selectStatus(page, "approved");
    await expect(page.getByTestId(`row-${packageName}`)).toBeHidden();

    // ...and present under the new one, which was cached before the decision.
    await selectStatus(page, "rejected");
    await expect(page.getByTestId(`row-${packageName}`)).toBeVisible();
  });

  // A role grants access by repository pattern, so the repository detail's
  // Permissions tab lists it. Nothing told that tab to look again.
  test("granting a role permission shows up on the repository Permissions tab", async ({
    signedInAs,
    adminApi,
  }) => {
    const roleName = scopedName("role");
    const repositoryId = await seededRepositoryId(adminApi);
    const permissionsTab = `/workspace/repositories/${repositoryId}/permissions`;

    const page = await signedInAs("admin");

    // Visit first so the tab's query is cached and a stale read would show.
    await page.goto(permissionsTab);
    await expect(page.getByRole("row", { name: roleName })).toBeHidden();

    await page.goto("/access/roles/new");
    await page.locator("#role-name").fill(roleName);
    await typeRepositoryPattern(page, "*");
    await page.getByRole("button", { name: "Add permission" }).click();
    await page.getByRole("button", { name: "Create role", exact: true }).click();
    await expect(page).toHaveURL(/\/access\/roles\/?$/);

    await page.goto(permissionsTab);
    await expect(page.getByRole("row", { name: roleName })).toBeVisible();
  });

  // A token's scopes name repositories by pattern, and the same tab lists the
  // tokens that reach the repository.
  test("issuing a token shows up on the repository Permissions tab", async ({
    signedInAs,
    adminApi,
  }) => {
    const tokenName = scopedName("tok");
    const repositoryId = await seededRepositoryId(adminApi);
    const permissionsTab = `/workspace/repositories/${repositoryId}/permissions`;

    const page = await signedInAs("admin");

    await page.goto(permissionsTab);
    await expect(page.getByRole("row", { name: tokenName })).toBeHidden();

    await page.goto("/workspace/tokens/new");
    await page.locator("#token-name").fill(tokenName);
    await page.locator("#token-description").fill("e2e");
    await page.locator("#expires-on").fill(oneMonthFromNow());
    await typeRepositoryPattern(page, "*");
    await page.getByRole("button", { name: "Add permission" }).click();
    await page.getByRole("button", { name: "Create token", exact: true }).click();
    // The secret is shown once; that panel is the acknowledgement it worked.
    await expect(page.getByText(/copy/i).first()).toBeVisible();

    await page.goto(permissionsTab);
    await expect(page.getByRole("row", { name: tokenName })).toBeVisible();
  });
});

// The status filter is a custom listbox, not a native select, so it is opened
// and picked rather than set.
async function selectStatus(page: Page, status: string) {
  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: status, exact: true }).click();
}

// An approval belongs to a proxy or hosted repository. Any seeded proxy will
// do; the test only needs somewhere for the rule to live.
async function proxyRepositoryName(adminApi: APIRequestContext): Promise<string> {
  const response = await adminApi.get("/api/v1/repositories");
  const repositories = (await response.json()) as { name: string; type: string }[];
  const proxy = repositories.find((repository) => repository.type === "proxy");

  if (!proxy) throw new Error("no seeded proxy repository to attach an approval to");

  return proxy.name;
}

function oneMonthFromNow(): string {
  const date = new Date();
  date.setMonth(date.getMonth() + 1);

  return date.toISOString().slice(0, 10);
}
