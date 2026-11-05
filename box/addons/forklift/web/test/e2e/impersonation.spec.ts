import type { APIRequestContext } from "@playwright/test";

import { expect, scopedName, test } from "./setup/fixtures";

// Acting as another user.
//
// Impersonation is safe to run alongside the rest: the server issues a new
// session cookie to this browser context only, so no other worker's session
// changes. What it does do is reload the whole app - every cached query belongs
// to the previous identity - so the flow is written as a navigation, not as a
// state change.
//
// The tests inside a describe run in order because the second depends on the
// first having started an impersonation.
test.describe.configure({ mode: "serial" });

test.describe("impersonation", () => {
  // Named inside the first test, not here: scopedName reads the running test's
  // own info, which does not exist while the describe body is being collected.
  // Serial mode is what lets the second test rely on it.
  let target = "";

  test("an administrator acts as another user and the banner says so", async ({
    signedInAs,
    adminApi,
  }) => {
    target = scopedName("imp");
    const created = await adminApi.post("/api/v1/users", {
      data: { username: target, password: "e2e-only-not-a-secret" },
    });
    expect(created.ok()).toBe(true);

    const page = await signedInAs("admin");
    await page.goto("/access/users");
    await page.getByTestId(`row-${target}`).getByRole("button", { name: "Modify" }).click();

    await page.getByRole("button", { name: /impersonate/i }).first().click();
    // The reason is recorded server-side and has a minimum length, so the
    // confirm button stays disabled until it is long enough - which is the
    // point of the field and worth asserting rather than just filling.
    const confirm = page.getByRole("button", { name: /start|impersonate/i }).last();
    await page.getByRole("textbox").last().fill("short");
    await expect(confirm).toBeDisabled();

    await page.getByRole("textbox").last().fill("e2e verifying the impersonation banner");
    await expect(confirm).toBeEnabled();
    await confirm.click();

    // The app reloads into the other identity; the banner is the only thing
    // that tells an administrator they are no longer themselves. "impersonated
    // by" is the half that cannot be mistaken for an ordinary session - the
    // username alone also appears on a normal sign-in.
    await expect(page.getByText(/impersonated by/i).first()).toBeVisible();
  });

  test("an impersonated session holds only the target's permissions", async ({
    signedInAs,
    adminApi,
  }) => {
    const page = await signedInAs("admin");
    await page.goto("/access/users");
    await page.getByTestId(`row-${target}`).getByRole("button", { name: "Modify" }).click();
    await page.getByRole("button", { name: /impersonate/i }).first().click();
    await page.getByRole("textbox").last().fill("e2e checking the permission drop");
    await page.getByRole("button", { name: /start|impersonate/i }).last().click();
    await expect(page.getByText(/impersonated by/i).first()).toBeVisible();

    // The target holds no role, so an administrative route must turn the
    // session away even though it was started by an administrator.
    await page.goto("/access/users");
    await expect(page).toHaveURL(/\/workspace\/repositories\/?$/);

    await adminApi.delete(`/api/v1/users/${await userId(adminApi, target)}`);
  });
});

async function userId(adminApi: APIRequestContext, username: string): Promise<number> {
  const response = await adminApi.get("/api/v1/users");
  const users = (await response.json()) as { id: number; username: string }[];
  const user = users.find((candidate) => candidate.username === username);

  if (!user) throw new Error(`e2e: user ${username} vanished before cleanup`);

  return user.id;
}
