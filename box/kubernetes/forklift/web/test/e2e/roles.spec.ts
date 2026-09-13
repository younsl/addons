import type { APIRequestContext } from "@playwright/test";

import { expect, scopedName, test } from "./setup/fixtures";

// The role round trip: created on one screen, listed on a second, edited on a
// third, deleted from there. Every step goes through the browser - a role
// created over the API would prove nothing about the form that creates it.
//
// Roles are named with scopedName because the suite is fullyParallel against a
// single backend, so the only row a test may reason about is its own.

test.describe("roles", () => {
  test("a role is created, its permissions edited, and deleted", async ({
    signedInAs,
    adminApi,
  }) => {
    const name = scopedName("role");
    const page = await signedInAs("admin");
    // Cleared before the test as well as after it. A run that dies partway
    // leaves its role behind, and every later run would then collide with it
    // on create - which is a confusing way to be told about an old failure.
    await deleteRoleIfPresent(adminApi, name);

    try {
      await page.goto("/access/roles/new");
      await page.locator("#role-name").fill(name);
      await page.locator("#role-description").fill("e2e round trip");
      // One permission at creation; the second is added on the detail page, so
      // both paths into a role's permissions are exercised.
      //
      // Escape after typing: the pattern combobox opens its suggestion list on
      // the first keystroke and that popup covers the button below it, so a
      // click would land on the list instead. This is what a person does too.
      await page.getByRole("combobox").first().fill("*");
      await page.keyboard.press("Escape");
      await page.getByRole("button", { name: "Add permission" }).click();
      await page.getByRole("button", { name: "Create role", exact: true }).click();

      await expect(page).toHaveURL(/\/access\/roles\/?$/);
      const row = page.getByTestId(`row-${name}`);
      await expect(row).toBeVisible();
      await expect(row).toContainText("*: read");

      await row.getByRole("button", { name: "Modify" }).click();
      await expect(page.getByTestId("page-role-detail")).toBeVisible();

      const permissions = page.getByTestId("panel-permissions");
      await permissions.getByRole("combobox").fill("maven-*");
      await page.keyboard.press("Escape");
      await permissions.getByRole("checkbox", { name: "write" }).click();
      await permissions.getByRole("button", { name: "Add", exact: true }).click();
      await expect(permissions.getByText("maven-*: read,write")).toBeVisible();

      // Exact text: "maven-*: read,write" contains "*: read" as a substring, so
      // a loose match would still find the permission this step removes.
      await permissions.getByRole("button", { name: "Remove permission" }).first().click();
      await expect(permissions.getByText("*: read", { exact: true })).toBeHidden();
      await expect(permissions.getByText("maven-*: read,write")).toBeVisible();

      await page.getByTestId("panel-danger-zone").getByRole("button", { name: "Delete role" })
        .click();
      await page.getByRole("alertdialog").getByRole("button", { name: "Delete", exact: true })
        .click();

      await expect(page).toHaveURL(/\/access\/roles\/?$/);
      await expect(page.getByTestId(`row-${name}`)).toBeHidden();
    } finally {
      await page.close();
      // The delete above is the last assertion, so a failure anywhere before it
      // would leave the role behind for every later run to trip over.
      await deleteRoleIfPresent(adminApi, name);
    }
  });

  // The E2E server loads this role from setup/rbac-policy.csv.
  test("a managed role has no edit controls", async ({ signedInAs, adminApi }) => {
    const managed = await findManagedRole(adminApi);
    expect(managed, "the managed RBAC fixture must be loaded").toBeDefined();

    const page = await signedInAs("admin");
    await page.goto(`/access/roles/${managed!.id}`);

    await expect(page.getByTestId("page-role-detail")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Managed role" })).toBeVisible();
    // The permission list is still shown; only the controls that would write
    // are gone, because the API answers those with a 409.
    await expect(page.getByTestId("panel-permissions")).toBeVisible();
    await expect(page.getByTestId("panel-permissions").getByRole("combobox")).toBeHidden();
    await expect(page.getByTestId("panel-danger-zone")).toBeHidden();
  });

  test("an auditor reads the role list without editing it", async ({ signedInAs, adminApi }) => {
    const name = scopedName("role");
    // Arranged over the API: this is the fixture the auditor looks at, not the
    // behaviour under test.
    const roleId = await createRole(adminApi, name);
    const page = await signedInAs("auditor");

    try {
      await page.goto("/access/roles");

      await expect(page.getByTestId("page-roles")).toBeVisible();
      await expect(page.getByTestId(`row-${name}`)).toBeVisible();
      await expect(page.getByRole("button", { name: "Create role" })).toBeHidden();

      await page.goto(`/access/roles/${roleId}`);

      await expect(page.getByTestId("panel-permissions")).toBeVisible();
      await expect(page.getByTestId("panel-permissions").getByRole("button", { name: "Add" }))
        .toBeHidden();
      await expect(page.getByTestId("panel-danger-zone")).toBeHidden();
    } finally {
      await page.close();
      await deleteRoleIfPresent(adminApi, name);
    }
  });
});

async function createRole(adminApi: APIRequestContext, name: string): Promise<number> {
  // Idempotent for the same reason the tests clear up front: a leftover from an
  // earlier failure would otherwise answer 409 here for good.
  await deleteRoleIfPresent(adminApi, name);
  const response = await adminApi.post("/api/v1/roles", {
    data: {
      name,
      description: "e2e fixture",
      permissions: [{ repo_pattern: "*", actions: ["read"] }],
    },
  });
  expect(response.ok()).toBe(true);

  return (await response.json()).id as number;
}

async function deleteRoleIfPresent(adminApi: APIRequestContext, name: string) {
  const role = await findRole(adminApi, (candidate) => candidate.name === name);
  if (role) await adminApi.delete(`/api/v1/roles/${role.id}`);
}

async function findManagedRole(adminApi: APIRequestContext) {
  return findRole(adminApi, (candidate) => Boolean(candidate.managed));
}

async function findRole(
  adminApi: APIRequestContext,
  match: (role: { id: number; name: string; managed?: boolean }) => boolean,
) {
  const response = await adminApi.get("/api/v1/roles");
  const roles = (await response.json()) as { id: number; name: string; managed?: boolean }[];

  return roles.find(match);
}
