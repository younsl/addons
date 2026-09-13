import type { APIRequestContext, Page } from "@playwright/test";

import { expect, scopedName, seededRepositoryId, SEEDED_REPOSITORY, test } from "./setup/fixtures";

// The repository round trip, once per type, plus the two refusals that matter:
// an upstream that does not answer must not stop a proxy being created, and a
// seeded repository must not appear to be deletable.
//
// Every repository is named with scopedName and deleted again: the suite is
// fullyParallel against one backend, and the directory is the screen most
// likely to be read by another worker while this one writes to it. Nothing here
// asserts a total row count for the same reason.

test.describe("repositories", () => {
  test("a hosted repository is created, configured, taken offline and deleted", async ({
    signedInAs,
    adminApi,
  }) => {
    const name = scopedName("repo");
    // Cleared before the test as well as after it. A run that dies partway
    // leaves the repository behind, and every later run would then collide with
    // it on create - which is a confusing way to be told about an old failure.
    await deleteRepositoryIfPresent(adminApi, name);
    const page = await signedInAs("admin");

    try {
      await page.goto("/workspace/repositories");
      await page.getByRole("button", { name: "New repository" }).click();

      await page.locator("#repository-name").fill(name);
      // Format stays at its default (Maven); the type is what changes the form.
      await page.getByRole("radio", { name: /Hosted/ }).click();
      await page.getByRole("button", { name: "Create repository" }).click();

      await expect(page).toHaveURL(/\/workspace\/repositories\/?$/);
      await expect(rowFor(page, name)).toBeVisible();

      await page.getByRole("link", { name, exact: true }).click();
      await expect(page.getByRole("heading", { name })).toBeVisible();

      // The tab strip, not the sidebar: both hold a link called "Settings", and
      // the sidebar's goes to the application preferences instead.
      await page.locator("nav").filter({ hasText: "Artifacts" })
        .getByRole("link", { name: "Settings" }).click();
      await expect(page).toHaveURL(/\/repositories\/\d+\/settings$/);

      // Retention is the only draft-edited setting a hosted repository has; its
      // input is identified by placeholder because the label is not bound to it.
      const idleTtl = page.getByPlaceholder("0", { exact: true });
      await idleTtl.fill("7d");
      await page.getByRole("button", { name: "Save changes" }).click();
      await expect(page.getByText("Saved.")).toBeVisible();

      // Reloaded rather than trusted: "Saved." is set from the mutation's own
      // success, so only a fresh read proves the value reached the database.
      //
      // It comes back as 168h0m0s, not as the 7d that was typed. Go's duration
      // formatter has no unit above the hour, so the server stores the value it
      // was given and hands back its own spelling of it. Worth knowing as a
      // user - an edit that reads differently after a reload looks like it was
      // not saved - but it is the same duration, so the assertion accepts
      // either rather than pinning the app to one side of a server decision.
      await page.reload();
      await expect(page.getByPlaceholder("0", { exact: true })).toHaveValue(/^(7d|168h0m0s)$/);

      // Offline is a server-state change rather than a draft edit, so the new
      // value has to come back through the query before the switch agrees.
      //
      // Named, not "the switch on the page": Settings also carries the
      // visibility toggle. The name matches either side of this one, since
      // clicking it is what changes its own label.
      const state = page.getByRole("switch", { name: /online|offline/i });
      await state.click();
      await expect(state).not.toBeChecked();
      await expect(page.getByText(/uploads and downloads are refused/)).toBeVisible();

      await state.click();
      await expect(state).toBeChecked();
      await expect(page.getByText(/online and serving the package protocols/)).toBeVisible();

      await page.getByRole("button", { name: "Delete repository" }).click();
      await page.getByRole("alertdialog").getByRole("button", { name: "Delete", exact: true })
        .click();

      await expect(page).toHaveURL(/\/workspace\/repositories\/?$/);
      await expect(rowFor(page, name)).toBeHidden();
    } finally {
      await page.close();
      // The delete is the last assertion, so a failure anywhere before it would
      // leave the repository behind for every later run to trip over.
      await deleteRepositoryIfPresent(adminApi, name);
    }
  });

  // A group is the one type whose row is not the whole story: its members are
  // listed only underneath it, never as top-level rows of their own, so the
  // nesting is the only place they appear at all.
  test("a group repository nests its members in the directory", async ({
    signedInAs,
    adminApi,
  }) => {
    const base = scopedName("grp");
    // Distinct suffixes rather than a shared prefix: a row is matched by its
    // text, and a member name that contains the group name would match both.
    const group = `${base}-group`;
    const [first, second] = [`${base}-a`, `${base}-b`];

    // The members are the fixture; creating a hosted repository through the
    // form is the first test's job, and doing it twice more here would only
    // make this one slower.
    for (const member of [first, second]) await createRepository(adminApi, member);
    await deleteRepositoryIfPresent(adminApi, group);
    const page = await signedInAs("admin");

    try {
      await page.goto("/workspace/repositories/new");
      await page.locator("#repository-name").fill(group);
      await page.getByRole("radio", { name: /Group/ }).click();

      // The member picker resets to its placeholder after each pick, which is
      // what makes the same control usable twice.
      for (const member of [first, second]) {
        await page.getByRole("combobox").filter({ hasText: "add member" }).click();
        await page.getByRole("option", { name: `${member} (hosted)` }).click();
      }
      await page.getByRole("button", { name: "Create repository" }).click();

      await expect(page).toHaveURL(/\/workspace\/repositories\/?$/);
      const groupRow = rowFor(page, group);
      // The count beside the name is what the directory says about a group
      // without being expanded.
      await expect(groupRow).toContainText("(2)");
      await expect(rowFor(page, first)).toBeVisible();
      await expect(rowFor(page, second)).toBeVisible();

      // Groups open by default, so collapsing is what proves the member rows
      // belong to this group rather than standing on their own.
      await groupRow.getByRole("button", { name: "Collapse group" }).click();

      await expect(rowFor(page, first)).toBeHidden();
      await expect(rowFor(page, second)).toBeHidden();
      await expect(groupRow).toBeVisible();
    } finally {
      await page.close();
      // The group first: a member cannot be left orphaned inside one.
      await deleteRepositoryIfPresent(adminApi, group);
      for (const member of [first, second]) await deleteRepositoryIfPresent(adminApi, member);
    }
  });

  // The connectivity probe is advisory. An upstream may be behind something not
  // yet routable, or simply down at the moment the form is filled in, and the
  // address is still the right one to configure - so the hint reports and the
  // create proceeds. A version of this that blocked would look like a safety
  // feature, which is why the passing create is asserted and not just the hint.
  test("an unreachable upstream is reported but does not block a proxy", async ({
    signedInAs,
    adminApi,
  }) => {
    const name = scopedName("repo");
    await deleteRepositoryIfPresent(adminApi, name);
    const page = await signedInAs("admin");

    try {
      await page.goto("/workspace/repositories/new");
      await page.locator("#repository-name").fill(name);
      await page.getByRole("radio", { name: /Proxy/ }).click();
      // Port 9 (discard) on the loopback: refused immediately rather than left
      // to time out, so the probe answers while the test is still watching.
      await page.locator("#upstream-url").fill("http://127.0.0.1:9/e2e-unreachable");

      // Longer than the default: the probe waits out a 600ms debounce on the
      // field before it is even sent.
      await expect(page.getByText(/Unreachable/)).toBeVisible({ timeout: 15_000 });

      const create = page.getByRole("button", { name: "Create repository" });
      await expect(create).toBeEnabled();
      await create.click();

      await expect(page).toHaveURL(/\/workspace\/repositories\/?$/);
      await expect(rowFor(page, name)).toBeVisible();
    } finally {
      await page.close();
      await deleteRepositoryIfPresent(adminApi, name);
    }
  });

  // Seeded repositories are recreated on the next startup, so deleting one only
  // appears to work. The server refuses with 403; the screen has to say so
  // rather than offering a button that fails.
  test("a seeded repository cannot be deleted", async ({ signedInAs, adminApi }) => {
    const id = await seededRepositoryId(adminApi);
    const page = await signedInAs("admin");

    await page.goto(`/workspace/repositories/${id}/settings`);

    await expect(page.getByRole("button", { name: "Delete repository" })).toBeDisabled();
    await expect(page.getByRole("heading", { name: "Deletion locked" })).toBeVisible();

    // The only mutation this file makes against a shared seeded repository, and
    // it is the assertion: without it the disabled button above could be hiding
    // a delete the server would happily have performed.
    const refused = await adminApi.delete(`/api/v1/repositories/${id}`);
    expect(refused.status()).toBe(403);
    await expect(page.getByRole("heading", { name: SEEDED_REPOSITORY })).toBeVisible();
  });

  // "reader" rather than "plain": an account with no grant at all is served an
  // empty directory and a 403 on any repository, so it could not open one to
  // check what a non-administrator sees on a page they may reach.
  test("a reader browses the directory without creating one", async ({ signedInAs }) => {
    const page = await signedInAs("reader");

    await page.goto("/workspace/repositories");

    await expect(page.getByRole("heading", { name: "Repositories" })).toBeVisible();
    await expect(page.getByRole("button", { name: "New repository" })).toBeHidden();

    await page.getByRole("link", { name: SEEDED_REPOSITORY, exact: true }).click();

    await expect(page).toHaveURL(/\/workspace\/repositories\/\d+/);
    await expect(page.getByRole("heading", { name: SEEDED_REPOSITORY })).toBeVisible();
  });
});

// The directory's rows carry no testid, so a row is found by the name it holds.
// Scoped to a name this test created, never to a count: another worker's
// repository lands in the same table.
function rowFor(page: Page, name: string) {
  return page.getByRole("row").filter({ hasText: name });
}

async function createRepository(adminApi: APIRequestContext, name: string) {
  // Idempotent for the same reason the tests clear up front: a leftover from an
  // earlier failure would otherwise answer 409 here for good.
  await deleteRepositoryIfPresent(adminApi, name);
  const response = await adminApi.post("/api/v1/repositories", {
    data: { name, format: "maven", type: "hosted" },
  });

  // The status is in the message: a bare "expected true" here says nothing
  // about whether the name collided, the payload was rejected, or the server
  // lost the row it had just written.
  if (!response.ok()) {
    throw new Error(`e2e: could not create ${name} (${response.status()}): ${await response.text()}`);
  }
}

async function deleteRepositoryIfPresent(adminApi: APIRequestContext, name: string) {
  const response = await adminApi.get("/api/v1/repositories");
  const repositories = (await response.json()) as { id: number; name: string }[];
  const repository = repositories.find((candidate) => candidate.name === name);

  if (repository) await adminApi.delete(`/api/v1/repositories/${repository.id}`);
}
