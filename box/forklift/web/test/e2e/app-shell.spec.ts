import type { APIRequestContext } from "@playwright/test";

import {
  createApprovalGatedRepository,
  expect,
  requestBlockedPackage,
  scopedName,
  SEEDED_REPOSITORY,
  test,
} from "./setup/fixtures";

// The frame every screen is drawn inside: global search, the sidebar's counters
// and collapse, the preferences that change the whole UI, and the way out.
//
// None of it belongs to a route, so none of it is covered by the smoke pass -
// and all of it is the kind of thing that breaks silently. A search dialog that
// never queries, a badge that stays at its first value, a collapse that forgets
// itself on reload: the page still renders, so only a browser notices.
//
// Nothing here asserts a total. The repository counter is checked for being a
// number that is there, not for being fifteen: another worker creating a
// repository would break that, and does.

test.describe("global search", () => {
  // Two ways in, one dialog. The keyboard shortcut is the one people use and
  // the one nothing else tests; the sidebar trigger is the one they discover.
  test("opens from the sidebar and from the keyboard, and asks for two characters", async ({
    signedInAs,
  }) => {
    const page = await signedInAs("admin");
    await page.goto("/workspace/repositories");

    await page.getByRole("button", { name: /Search/ }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();

    // One character is below the threshold: the dialog says so instead of
    // running a query that would match most of the instance.
    await dialog.getByRole("textbox").fill("m");
    await expect(dialog.getByText(/at least 2 characters/i)).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();

    await page.keyboard.press("ControlOrMeta+k");
    await expect(page.getByRole("dialog")).toBeVisible();
  });

  // The results are grouped by what they are, and each row knows where it
  // goes - a hit that lists but does not navigate is the failure worth pinning,
  // because the list looks right either way.
  test("two characters return grouped results, and a result opens its page", async ({
    signedInAs,
    adminApi,
  }) => {
    const packageName = scopedName("hit");
    const repository = await proxyRepositoryName(adminApi);
    // Arranged through the API so the term is unique to this worker: searching
    // for something the whole suite shares would rank against other workers'
    // rows.
    const created = await adminApi.post("/api/v1/approvals", {
      data: { repo: repository, package: packageName, status: "approved", note: "e2e fixture" },
    });
    expect(created.ok()).toBe(true);

    const page = await signedInAs("admin");
    await page.goto("/workspace/repositories");
    // Waited for before the shortcut is pressed: the key listener is installed
    // by the sidebar, so a press that lands before the shell has mounted is
    // simply lost and the dialog never opens.
    await expect(page.getByRole("button", { name: /Search/ })).toBeVisible();
    await page.keyboard.press("ControlOrMeta+k");

    const dialog = page.getByRole("dialog");
    await dialog.getByRole("textbox").fill(SEEDED_REPOSITORY);
    // The section heading is what makes this a grouped list rather than a flat
    // one; scoped to the dialog because the sidebar has a "Repositories" link.
    await expect(dialog.getByText("Repositories", { exact: true })).toBeVisible();
    await expect(dialog.getByRole("button", { name: new RegExp(SEEDED_REPOSITORY) })).toBeVisible();

    await dialog.getByRole("textbox").fill(packageName);
    await expect(dialog.getByText("Approvals", { exact: true })).toBeVisible();
    await dialog.getByRole("button", { name: new RegExp(packageName) }).click();

    await expect(page).toHaveURL(/\/workspace\/approvals\/\d+$/);
    await expect(page.getByTestId("page-approval-detail")).toBeVisible();
  });
});

test.describe("the sidebar", () => {
  // Both counters come from queries the sidebar owns rather than from the page,
  // so they are the one place a permission gate and a live number meet.
  //
  // The pending badge only renders above zero, and the API can only record
  // decided approvals - so this brings its own repository with the approval
  // gate on, asks it for a package, and deletes it again. That is also the
  // reason the count is read as "more than none" rather than as a number:
  // other workers queue their own requests.
  test("counts the repositories, and badges pending approvals only for a reviewer", async ({
    signedInAs,
    adminApi,
  }) => {
    const repository = scopedName("nav");
    const repositoryId = await createApprovalGatedRepository(adminApi, repository);

    try {
      await requestBlockedPackage(adminApi, repository, `${scopedName("wait")}.bin`);

      const page = await signedInAs("admin");
      await page.goto("/workspace/repositories");

      await expect(page.getByTestId("value-nav-repository-count")).toHaveText(/^\d+$/);
      await expect(page.getByTestId("value-nav-pending-count")).toHaveText(/^\d+$/);

      // An auditor may open the approval queue but may not decide anything in
      // it, so the sidebar does not offer it - and the badge for work waiting
      // to be reviewed belongs to whoever can review it.
      const auditor = await signedInAs("auditor");
      await auditor.goto("/workspace/repositories");

      await expect(auditor.getByTestId("value-nav-repository-count")).toBeVisible();
      await expect(auditor.getByTestId("value-nav-pending-count")).toBeHidden();
    } finally {
      await adminApi.delete(`/api/v1/repositories/${repositoryId}`);
    }
  });

  // Collapsing is stored, so it has to survive a reload. It is also the one
  // preference with no screen to set it from: the toggle is the whole feature.
  test("collapsing hides the labels and is remembered across a reload", async ({ signedInAs }) => {
    const page = await signedInAs("admin");
    await page.goto("/workspace/repositories");

    const count = page.getByTestId("value-nav-repository-count");
    await expect(count).toBeVisible();

    await page.getByRole("button", { name: "Collapse sidebar" }).click();

    // The rail keeps the icons and drops everything that needs width. The
    // counter is the label-side element with a handle on it.
    await expect(count).toBeHidden();
    await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();

    await page.reload();

    await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
    await expect(count).toBeHidden();

    // Left expanded again. The context is thrown away with the test, but a
    // stored preference is exactly the kind of thing that should not be
    // abandoned in a changed state.
    await page.getByRole("button", { name: "Expand sidebar" }).click();
    await expect(count).toBeVisible();
  });
});

test.describe("preferences", () => {
  // Language is applied from a store the whole UI reads, not from a reload, so
  // the proof is that text already on screen changes.
  //
  // English is restored before the test ends. Every other test's page is
  // created with the preference pinned to English, so a fresh page would be
  // safe anyway - but leaving a shared browser in Korean is the kind of thing
  // that is only found three failures later.
  test("changing the language changes the text on screen", async ({ signedInAs }) => {
    const page = await signedInAs("admin");
    await page.goto("/settings");
    await expect(page.getByTestId("page-settings")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();

    await page.locator("#language").click();
    await page.getByRole("option", { name: "Korean" }).click();

    await expect(page.getByRole("heading", { name: "설정" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Settings" })).toBeHidden();

    // The options are in the new language now, which is itself the point.
    await page.locator("#language").click();
    await page.getByRole("option", { name: "영어" }).click();
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  });
});

// Signing out ends this browser context's session only: the cookie is signed
// and stateless, so no other worker sharing the same account is affected.
test("signing out returns to the login screen and protected routes follow", async ({
  signedInAs,
}) => {
  const page = await signedInAs("admin");
  await page.goto("/workspace/repositories");

  await page.getByRole("button", { name: "Log Out" }).click();

  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();

  // The redirect is the whole point of the shell's auth gate: without it a
  // signed-out tab keeps rendering whatever it had cached.
  await page.goto("/workspace/approvals");
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();
});

async function proxyRepositoryName(adminApi: APIRequestContext): Promise<string> {
  const response = await adminApi.get("/api/v1/repositories");
  const repositories = (await response.json()) as { name: string; type: string }[];
  const proxy = repositories.find((repository) => repository.type === "proxy");

  if (!proxy) throw new Error("no seeded proxy repository to attach an approval to");

  return proxy.name;
}
