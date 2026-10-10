import type { APIRequestContext } from "@playwright/test";

import { E2E_PASSWORD } from "./setup/accounts";
import { expect, scopedName, test } from "./setup/fixtures";

// The user round trip. Everything an admin can do to an account lives on its
// detail page - role mapping, password, status, deletion - so one test walks
// the whole page rather than opening it four times.
//
// Names come from scopedName: the suite is fullyParallel against one backend,
// so a test may only reason about the row it created itself.

test.describe("users", () => {
  test("a local user is created, given a role, reset, disabled and deleted", async ({
    signedInAs,
    adminApi,
  }) => {
    const username = scopedName("user");
    const roleName = scopedName("role");
    // The role is a fixture: assigning one is the behaviour under test, making
    // one is the roles spec's job.
    await createRole(adminApi, roleName);
    // Cleared before the test as well as after it. A run that dies partway
    // leaves its user behind, and every later run would then collide with it
    // on create - which is a confusing way to be told about an old failure.
    await deleteUserIfPresent(adminApi, username);
    const page = await signedInAs("admin");

    try {
      await page.goto("/access/users/new");
      await page.locator("#username").fill(username);
      // Both password fields carry a testid: "Confirm password" also satisfies
      // an accessible-name lookup for "Password", so the names are ambiguous.
      await page.getByTestId("field-password").fill(E2E_PASSWORD);
      await page.getByTestId("field-confirm-password").fill(E2E_PASSWORD);
      await page.getByRole("button", { name: "Create user" }).click();

      // Longer than the default: hashing a password is deliberately slow, and
      // several workers are asking this one server to do it at once.
      await expect(page).toHaveURL(/\/access\/users\/?$/, { timeout: 15_000 });
      const row = page.getByTestId(`row-${username}`);
      await expect(row).toBeVisible();

      await row.getByRole("button", { name: "Modify" }).click();
      await expect(page.getByTestId("page-user-detail")).toBeVisible();

      const roles = page.getByTestId("panel-roles");
      await expect(roles.getByText("No roles assigned.")).toBeVisible();
      await roles.getByRole("combobox").click();
      await page.getByRole("option", { name: roleName }).click();
      await roles.getByRole("button", { name: "Add", exact: true }).click();
      await expect(roles.getByText(roleName)).toBeVisible();

      await roles.getByRole("button", { name: "Remove role" }).click();
      await expect(roles.getByText("No roles assigned.")).toBeVisible();

      // A local account is the only kind with a password to reset; the panel is
      // absent for OIDC users and robots.
      const password = page.getByTestId("panel-password");
      await password.getByRole("textbox").fill(`${E2E_PASSWORD}-rotated`);
      await password.getByRole("button", { name: "Reset password" }).click();
      await expect(password.getByText("Password updated.")).toBeVisible({ timeout: 15_000 });

      // Disabling is the reversible alternative to deletion, so it is only
      // proven by turning it back on afterwards.
      const status = page.getByTestId("panel-status");
      await status.getByRole("switch").click();
      await expect(status.getByText("Account disabled")).toBeVisible();
      await status.getByRole("switch").click();
      await expect(status.getByText("Account active")).toBeVisible();

      await page.getByTestId("panel-danger-zone").getByRole("button", { name: "Delete user" })
        .click();
      await page.getByRole("alertdialog").getByRole("button", { name: "Delete", exact: true })
        .click();

      await expect(page).toHaveURL(/\/access\/users\/?$/);
      await expect(page.getByTestId(`row-${username}`)).toBeHidden();
    } finally {
      await page.close();
      // The delete is the last assertion, so any earlier failure leaves both
      // the user and its role behind for the next run to trip over.
      await deleteUserIfPresent(adminApi, username);
      await deleteRoleIfPresent(adminApi, roleName);
    }
  });

  // A robot authenticates only by token, so the form must not ask for a
  // password - and the detail page must not offer to reset one.
  test("a robot account is created without a password", async ({ signedInAs, adminApi }) => {
    const username = scopedName("robot");
    await deleteUserIfPresent(adminApi, username);
    const page = await signedInAs("admin");

    try {
      await page.goto("/access/users/new");
      await expect(page.getByTestId("field-password")).toBeVisible();

      await page.getByRole("radio", { name: /Robot Account/ }).click();

      await expect(page.getByTestId("field-password")).toBeHidden();
      await expect(page.getByTestId("field-confirm-password")).toBeHidden();

      await page.locator("#username").fill(username);
      await page.getByRole("button", { name: "Create user" }).click();

      await expect(page).toHaveURL(/\/access\/users\/?$/);
      const row = page.getByTestId(`row-${username}`);
      await expect(row).toBeVisible();
      await expect(row).toContainText("Robot Account");

      await row.getByRole("button", { name: "Modify" }).click();
      await expect(page.getByTestId("page-user-detail")).toBeVisible();
      await expect(page.getByTestId("panel-password")).toBeHidden();
    } finally {
      await page.close();
      await deleteUserIfPresent(adminApi, username);
    }
  });

  test("an auditor reads the user list without editing it", async ({ signedInAs, adminApi }) => {
    const username = scopedName("user");
    const userId = await createUser(adminApi, username);
    const page = await signedInAs("auditor");

    try {
      await page.goto("/access/users");

      await expect(page.getByTestId("page-users")).toBeVisible();
      await expect(page.getByTestId(`row-${username}`)).toBeVisible();
      await expect(page.getByRole("button", { name: "Create user" })).toBeHidden();

      await page.goto(`/access/users/${userId}`);

      // The summary and roles panels are readable; every panel that writes -
      // password, lockout, status, danger zone - is gated on me.admin.
      await expect(page.getByTestId("panel-summary")).toBeVisible();
      await expect(page.getByTestId("panel-roles").getByRole("combobox")).toBeHidden();
      await expect(page.getByTestId("panel-password")).toBeHidden();
      await expect(page.getByTestId("panel-lockout")).toBeHidden();
      await expect(page.getByTestId("panel-status")).toBeHidden();
      await expect(page.getByTestId("panel-danger-zone")).toBeHidden();
    } finally {
      await page.close();
      await deleteUserIfPresent(adminApi, username);
    }
  });
});

// Both fixtures delete before they create: a leftover from an earlier failure
// would otherwise answer 409 here for good.
async function createRole(adminApi: APIRequestContext, name: string): Promise<number> {
  await deleteRoleIfPresent(adminApi, name);
  const response = await adminApi.post("/api/v1/roles", {
    data: { name, description: "e2e fixture", permissions: [{ repo_pattern: "*", actions: ["read"] }] },
  });
  expect(response.ok()).toBe(true);

  return (await response.json()).id as number;
}

// Retried once. Creating a user concurrently with other writers occasionally
// answers 404: the server reads the row back on a second pooled connection
// straight after inserting it, and sometimes does not find it. That is a server
// race, not something this test is about - it only needs an account to look at,
// so it asks again rather than reporting a failure in the auditor's screens.
async function createUser(adminApi: APIRequestContext, username: string): Promise<number> {
  for (let attempt = 0; ; attempt++) {
    await deleteUserIfPresent(adminApi, username);
    const response = await adminApi.post("/api/v1/users", {
      data: { username, password: E2E_PASSWORD },
    });

    if (response.ok()) return (await response.json()).id as number;
    if (attempt === 1) {
      expect(response.ok(), `POST /api/v1/users: ${response.status()}`).toBe(true);
    }
  }
}

async function deleteUserIfPresent(adminApi: APIRequestContext, username: string) {
  const response = await adminApi.get("/api/v1/users");
  const users = (await response.json()) as { id: number; username: string }[];
  const user = users.find((candidate) => candidate.username === username);

  if (user) await adminApi.delete(`/api/v1/users/${user.id}`);
}

async function deleteRoleIfPresent(adminApi: APIRequestContext, name: string) {
  const response = await adminApi.get("/api/v1/roles");
  const roles = (await response.json()) as { id: number; name: string }[];
  const role = roles.find((candidate) => candidate.name === name);

  if (role) await adminApi.delete(`/api/v1/roles/${role.id}`);
}
