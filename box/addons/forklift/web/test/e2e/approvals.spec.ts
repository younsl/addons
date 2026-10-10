import type { APIRequestContext, Page } from "@playwright/test";

import {
  createApprovalGatedRepository,
  expect,
  requestBlockedPackage,
  scopedName,
  test,
} from "./setup/fixtures";

// The approval workflow: recording a rule ahead of demand, deciding a request
// that arrived, and the deny list that sits beside the queue.
//
// Nothing here runs serially. Every package, deny and repository is named with
// scopedName, so two workers writing to the same shared backend never touch the
// same row, and no assertion reads a total - only the caller's own
// `row-<name>`. The one test that needs a queue of its own creates a
// repository, uses it, and deletes it again.
//
// Two things about this screen cost time to find out, so they are stated once
// here rather than rediscovered per test:
//
//   1. "Review" in the queue is a link in button's clothing: it navigates to
//      the request's own page, and the review modal is opened by a *second*
//      "Review" button there. A test that clicks once and looks for the modal
//      waits forever.
//   2. AddRuleModal is one form over two endpoints. Without a version it writes
//      an approval decision (the queue); with a version and "block" it writes a
//      version deny (a different list, on a different screen). Getting that
//      wrong is invisible unless the test looks in the list the row should not
//      be in.

test.describe("rules recorded ahead of demand", () => {
  // Pre-approval is the reason this form exists: a package nobody has requested
  // yet is admitted in advance, and the queue is where that decision shows up.
  test("a rule that pre-approves a package lands in the queue as approved", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("allow");
    const repository = await approvableRepository(adminApi);

    const page = await signedInAs("admin");
    await page.goto("/workspace/approvals");
    await addRule(page, { repo: repository.name, package: packageName, decision: "allow" });

    // Searched rather than scrolled: the queue is shared and paged at 50, so
    // another worker's rows can push this one off the first page.
    await searchQueue(page, packageName);
    await selectStatus(page, "approved");
    await expect(page.getByTestId(`row-${packageName}`)).toBeVisible();

    // The status the rule did not choose stays empty, which is what makes the
    // assertion above about "approved" rather than about "listed at all".
    await selectStatus(page, "rejected");
    await expect(page.getByTestId(`row-${packageName}`)).toBeHidden();
  });

  test("a rule that blocks a package lands in the queue as rejected", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("block");
    const repository = await approvableRepository(adminApi);

    const page = await signedInAs("admin");
    await page.goto("/workspace/approvals");
    await addRule(page, { repo: repository.name, package: packageName, decision: "block" });

    await searchQueue(page, packageName);
    await selectStatus(page, "rejected");
    await expect(page.getByTestId(`row-${packageName}`)).toBeVisible();

    await selectStatus(page, "approved");
    await expect(page.getByTestId(`row-${packageName}`)).toBeHidden();
  });

  // The same form, one field further: filling in a version switches the
  // submission from POST /approvals to POST /version-denies. Both succeed and
  // both close the modal, so the only way to catch the wrong one is to check
  // the list it must not be in as well as the one it must.
  test("blocking one version writes a deny, not a queue entry", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("deny");
    const version = "9.9.9";
    const repository = await approvableRepository(adminApi);

    const page = await signedInAs("admin");
    await page.goto("/workspace/approvals");
    await addRule(page, {
      repo: repository.name,
      package: packageName,
      decision: "block",
      version,
    });

    // Not in the queue under any status - a package-level rejection would show
    // here, and that is exactly the mistake being guarded against.
    await searchQueue(page, packageName);
    await selectStatus(page, "all statuses");
    await expect(page.getByTestId(`row-${packageName}`)).toBeHidden();

    await openBlockedVersions(page, repository.id);
    await expect(page.getByTestId(`row-${packageName}@${version}`)).toBeVisible();
  });

  // Removing a deny serves the version again, so it is a real decision rather
  // than tidying up: the row has to leave the list for the operator to believe
  // it took effect.
  test("a version deny is removed from the deny list", async ({ signedInAs, adminApi }) => {
    const packageName = scopedName("undeny");
    const version = "1.2.3";
    const repository = await approvableRepository(adminApi);

    // Arranged through the API: creating the deny is covered above, and this
    // test is about the removal.
    const created = await adminApi.post("/api/v1/version-denies", {
      data: {
        repo: repository.name,
        package: packageName,
        version,
        reason: "e2e fixture",
      },
    });
    expect(created.ok()).toBe(true);

    const page = await signedInAs("admin");
    await openBlockedVersions(page, repository.id);

    const row = page.getByTestId(`row-${packageName}@${version}`);
    await expect(row).toBeVisible();
    await row.getByRole("button", { name: "Remove" }).click();
    await page.getByRole("alertdialog").getByRole("button", { name: "Remove", exact: true }).click();

    await expect(row).toBeHidden();
  });
});

test.describe("deciding a request", () => {
  // The decision is made on the request's own page, where the vulnerability
  // analysis a reviewer is supposed to read also lives. The status badge in the
  // header is the acknowledgement; the modal closing only means the request
  // returned.
  test("a decision on the detail page moves the status badge", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("decide");
    const repository = await approvableRepository(adminApi);
    const approvalId = await createApproval(adminApi, repository.name, packageName, "approved");

    const page = await signedInAs("admin");
    await page.goto(`/workspace/approvals/${approvalId}`);

    const detail = page.getByTestId("page-approval-detail");
    await expect(detail).toBeVisible();
    await expect(statusBadge(page)).toHaveText("approved");
    // The panels a reviewer decides from, not decoration: if they fail to
    // render the decision is being made blind.
    await expect(page.getByTestId("panel-request")).toBeVisible();
    await expect(page.getByTestId("panel-vuln-analysis")).toBeVisible();

    await detail.getByRole("button", { name: "Review" }).click();
    await page.getByTestId("action-reject").click();

    await expect(statusBadge(page)).toHaveText("rejected");
  });

  // From the queue it takes two clicks, because the first one navigates. This
  // pins the route the reviewer actually walks.
  test("the queue's Review button opens the request before anything is decided", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("route");
    const repository = await approvableRepository(adminApi);
    await createApproval(adminApi, repository.name, packageName, "approved");

    const page = await signedInAs("admin");
    await page.goto("/workspace/approvals");
    await searchQueue(page, packageName);
    await selectStatus(page, "approved");

    await page.getByTestId(`row-${packageName}`).getByRole("button", { name: "Review" }).click();

    await expect(page).toHaveURL(/\/workspace\/approvals\/\d+$/);
    await expect(page.getByTestId("page-approval-detail")).toBeVisible();
    // Still approved: navigating is not deciding.
    await expect(statusBadge(page)).toHaveText("approved");
  });

  // An approver holds no admin rights at all, so the queue has to work without
  // the user and repository listings it would like to link to.
  test("an approver decides", async ({ signedInAs, adminApi }) => {
    const packageName = scopedName("appr");
    const repository = await approvableRepository(adminApi);
    const approvalId = await createApproval(adminApi, repository.name, packageName, "approved");

    const page = await signedInAs("approver");
    await page.goto(`/workspace/approvals/${approvalId}`);

    await page.getByTestId("page-approval-detail").getByRole("button", { name: "Review" }).click();
    await page.getByTestId("action-reject").click();

    await expect(statusBadge(page)).toHaveText("rejected");
  });

  // The auditor is the read-only half of the same route: it may open every
  // screen an approver can, and act on none of them. A gate wired to
  // canViewApprovalQueue where it meant canReviewApprovals would show the
  // buttons here and only fail at the server.
  test("an auditor reads the queue but is offered no decision", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("audit");
    const repository = await approvableRepository(adminApi);
    const approvalId = await createApproval(adminApi, repository.name, packageName, "approved");

    const page = await signedInAs("auditor");
    await page.goto("/workspace/approvals");

    await expect(page.getByTestId("page-approvals")).toBeVisible();
    await searchQueue(page, packageName);
    await selectStatus(page, "approved");
    await expect(page.getByTestId(`row-${packageName}`)).toBeVisible();
    await expect(page.getByRole("button", { name: "Add rule" })).toBeHidden();

    await page.goto(`/workspace/approvals/${approvalId}`);
    await expect(page.getByTestId("panel-request")).toBeVisible();
    // The detail page's own Review button is the one that opens the decision
    // modal; without it there is no way to decide from here.
    await expect(page.getByRole("button", { name: "Review" })).toBeHidden();
  });
});

// The bulk screen is one row per repository with a pending queue, and a single
// toggle that changes what "approve" would mean on every one of them.
//
// The approval itself is never pressed: a bulk approve cannot be undone and the
// seeded repositories are shared with every other worker. So this test brings
// its own repository, queues one request in it, reads the row, and deletes the
// repository again - which is also the only way to get a pending row at all,
// since the API can only record decided ones. Pending is what a blocked fetch
// leaves behind.
test("the bulk screen counts a repository's queue, and Clean-only re-counts it", async ({
  signedInAs,
  adminApi,
}) => {
  const repository = scopedName("bulk");
  const packageName = `${scopedName("pending")}.bin`;
  const repositoryId = await createApprovalGatedRepository(adminApi, repository);

  const page = await signedInAs("admin");
  try {
    await requestBlockedPackage(adminApi, repository, packageName);
    await page.goto("/workspace/approvals/bulk");
    await expect(page.getByTestId("page-bulk-approval")).toBeVisible();

    const row = page.getByTestId(`row-${repository}`);
    await expect(row).toBeVisible();
    // One pending package, so the button says what pressing it would admit.
    await expect(row.getByRole("button", { name: "Approve 1" })).toBeVisible();

    // Clean-only counts scans instead of requests. A raw repository has no OSV
    // ecosystem, so nothing in it can ever be Clean - which makes the count
    // fall to zero and the button say so rather than lie about the blast
    // radius.
    await page.getByRole("switch").click();
    await expect(row.getByRole("button", { name: "No Clean" })).toBeVisible();
    await expect(row.getByRole("button", { name: "Approve 1" })).toBeHidden();
  } finally {
    await page.close();
    // Deleting the repository takes its approvals with it, so the shared
    // pending count goes back to what it was.
    await adminApi.delete(`/api/v1/repositories/${repositoryId}`);
  }
});

// The status filter is a custom listbox, not a native select, and it is the
// first one on the page (the repository filter follows it).
async function selectStatus(page: Page, status: string) {
  await page.getByRole("combobox").first().click();
  await page.getByRole("option", { name: status, exact: true }).click();
}

// The queue's search box is server-side and debounced; the assertion that
// follows waits it out.
async function searchQueue(page: Page, term: string) {
  await page.getByPlaceholder(/search all columns/i).fill(term);
}

// AddRuleModal, driven the way an operator drives it. The modal is found by its
// own submit button rather than by position: the page behind it carries two
// more comboboxes, and counting them from the document would break the moment a
// filter is added.
async function addRule(
  page: Page,
  {
    repo,
    package: packageName,
    decision,
    version,
  }: { repo: string; package: string; decision: "allow" | "block"; version?: string },
) {
  await page.getByRole("button", { name: "Add rule" }).click();
  const form = page.locator("form").filter({ has: page.getByTestId("action-save-rule") });

  await form.getByRole("combobox").first().click();
  await page.getByRole("option", { name: repo, exact: true }).click();
  await form.getByPlaceholder(/lodash/).fill(packageName);

  if (decision === "block") {
    await form.getByRole("combobox").nth(1).click();
    await page.getByRole("option", { name: "block", exact: true }).click();
  }
  // The version field only exists for a block, which is the whole point: there
  // is nothing to narrow about an approval.
  if (version) await form.getByPlaceholder(/4\.17\.99/).fill(version);

  await page.getByTestId("action-save-rule").click();
  // The modal closes on success and stays open with an error otherwise, so its
  // disappearance is the acknowledgement.
  await expect(page.getByTestId("action-save-rule")).toBeHidden();
}

// The deny list has no screen of its own: it is one panel of the repository's
// security policy, shown only while that step of the pipeline is selected.
async function openBlockedVersions(page: Page, repositoryId: number) {
  await page.goto(`/workspace/repositories/${repositoryId}/security`);
  await page.getByRole("button", { name: /Blocked versions/ }).click();
  await expect(page.getByRole("heading", { name: "Blocked versions" })).toBeVisible();
}

// The approval status badge in the detail page header. Matched by the three
// values it can hold, because the badge carries no testid and "approved" also
// appears in prose on the same screen.
function statusBadge(page: Page) {
  return page
    .getByTestId("page-approval-detail")
    .getByText(/^(pending|approved|rejected)$/)
    .first();
}

// An approval belongs to a proxy or hosted repository; a group has no upstream
// of its own to approve against. Any seeded proxy will do - the tests here name
// their own packages, so they never collide inside it.
async function approvableRepository(
  adminApi: APIRequestContext,
): Promise<{ id: number; name: string }> {
  const response = await adminApi.get("/api/v1/repositories");
  const repositories = (await response.json()) as { id: number; name: string; type: string }[];
  const proxy = repositories.find((repository) => repository.type === "proxy");

  if (!proxy) throw new Error("no seeded proxy repository to attach an approval to");

  return { id: proxy.id, name: proxy.name };
}

async function createApproval(
  adminApi: APIRequestContext,
  repo: string,
  packageName: string,
  status: "approved" | "rejected",
): Promise<number> {
  const response = await adminApi.post("/api/v1/approvals", {
    data: { repo, package: packageName, status, note: "e2e fixture" },
  });
  expect(response.ok()).toBe(true);

  return (await response.json()).id;
}
