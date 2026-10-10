import type { APIRequestContext, Page } from "@playwright/test";

import { accountFor, type E2ERole } from "./setup/accounts";
import { expect, scopedName, SEEDED_REPOSITORY, test, typeRepositoryPattern } from "./setup/fixtures";

// The token round trip, and the two things about tokens that only a browser can
// prove: the secret exists on screen exactly once, and the quota number beside
// the table moves with the table.
//
// Every test signs in as a different account. Tokens are capped at three per
// user (MAX_TOKENS_PER_USER, enforced server-side with a 409), so two workers
// creating tokens as the same user would eventually fail the create rather than
// the assertion. The admin is deliberately left alone: freshness.spec.ts issues
// a token as the admin and leaves it behind.
//
// The secret is a hex string behind a fixed prefix (auth.GenerateToken). Pinned
// as a shape rather than as "some text": a panel rendering an empty string, or
// the token's name, would otherwise pass.
const SECRET = /^flpat_[0-9a-f]{48}$/;

test.describe("tokens", () => {
  test("a token is created, shown once, rescoped and revoked", async ({
    signedInAs,
    adminApi,
  }) => {
    const name = scopedName("tok");
    // Cleared before the test as well as after it. A run that dies partway
    // leaves its token behind, and it counts against the three-token quota of
    // every later run.
    await deleteTokenIfPresent(adminApi, "security", name);
    const page = await signedInAs("security");

    try {
      await page.goto("/workspace/tokens");
      await page.getByRole("button", { name: "New token" }).click();

      await expect(page.getByTestId("page-token-new")).toBeVisible();
      await page.locator("#token-name").fill(name);
      await page.locator("#token-description").fill("e2e round trip");
      await page.locator("#expires-on").fill(isoDaysFromNow(30));
      // Enter commits the pattern; see typeRepositoryPattern for why typing it
      // is not enough. Actions default to read, so the scope is complete here.
      await typeRepositoryPattern(page, SEEDED_REPOSITORY);
      await page.getByRole("button", { name: "Add permission" }).click();
      await page.getByRole("button", { name: "Create token", exact: true }).click();

      // The form is replaced rather than navigated away from, because no
      // endpoint returns the secret again: if it is not read here it is gone.
      const created = page.getByTestId("page-token-created");
      await expect(created).toBeVisible();
      await expect(created.getByText(SECRET)).toBeVisible();
      await expect(created.getByText("Copy this token now. It will not be shown again."))
        .toBeVisible();

      await created.getByRole("button", { name: "Done" }).click();

      await expect(page).toHaveURL(/\/workspace\/tokens\/?$/);
      const row = page.getByTestId(`row-${name}`);
      await expect(row).toBeVisible();
      await expect(row).toContainText(`${SEEDED_REPOSITORY}: read`);

      await row.getByRole("button", { name: "Edit" }).click();

      // The modal is the only thing on this page carrying a repository-pattern
      // field, an "Add permission" button or a "Save", so these are unambiguous
      // at page level. It is a hand-rolled overlay, not a dialog, so there is no
      // role to scope into.
      await expect(page.getByRole("heading", { name: "Edit permissions" })).toBeVisible();
      await typeRepositoryPattern(page, "npm-*");
      await page.getByRole("button", { name: "Add permission" }).click();
      await page.getByRole("button", { name: "Save", exact: true }).click();

      await expect(page.getByRole("heading", { name: "Edit permissions" })).toBeHidden();
      // The list re-reads the token, so the new scope arriving here is also the
      // proof that the save reached the server.
      await expect(row).toContainText("npm-*: read");
      await expect(row).toContainText(`${SEEDED_REPOSITORY}: read`);

      await row.getByRole("button", { name: "Revoke" }).click();
      await page.getByRole("alertdialog").getByRole("button", { name: "Revoke" }).click();

      await expect(page.getByTestId(`row-${name}`)).toBeHidden();
    } finally {
      await page.close();
      // The revoke is the last assertion, so any earlier failure leaves the
      // token holding one of the account's three quota slots.
      await deleteTokenIfPresent(adminApi, "security", name);
    }
  });

  // The quota card and the table are two reads of the same fact. They were
  // wired up separately, so the count is only true if both move together.
  test("the quota usage follows a create and a revoke", async ({ signedInAs, adminApi }) => {
    const name = scopedName("tok");
    await deleteTokenIfPresent(adminApi, "approver", name);
    const page = await signedInAs("approver");

    try {
      await page.goto("/workspace/tokens");
      // A starting number rather than a fixed one: this account may hold a
      // token left behind by a failed earlier test, and the delta is what the
      // panel is being asked about anyway.
      const used = page.getByTestId("panel-quotas")
        .getByRole("row")
        .filter({ hasText: "Access tokens" })
        // Quota name, description, type, current usage, allocated quota.
        .getByRole("cell")
        .nth(3);
      await expect(used).toBeVisible();
      const before = Number(await used.innerText());

      await createTokenThroughUI(page, name, "*");

      await expect(page).toHaveURL(/\/workspace\/tokens\/?$/);
      await expect(page.getByTestId(`row-${name}`)).toBeVisible();
      await expect(used).toHaveText(String(before + 1));

      await page.getByTestId(`row-${name}`).getByRole("button", { name: "Revoke" }).click();
      await page.getByRole("alertdialog").getByRole("button", { name: "Revoke" }).click();

      await expect(page.getByTestId(`row-${name}`)).toBeHidden();
      await expect(used).toHaveText(String(before));
    } finally {
      await page.close();
      await deleteTokenIfPresent(adminApi, "approver", name);
    }
  });

  // The API caps a token's lifetime at one year and the field mirrors that, so
  // an out-of-range date never reaches the server. Nothing is created here, so
  // it runs as the admin without spending one of anybody's quota slots.
  test("an expiry beyond a year is refused by the date field", async ({ signedInAs }) => {
    const page = await signedInAs("admin");

    await page.goto("/workspace/tokens/new");
    await page.locator("#token-name").fill(scopedName("tok"));
    await page.locator("#token-description").fill("e2e expiry cap");
    await typeRepositoryPattern(page, "*");
    await page.getByRole("button", { name: "Add permission" }).click();

    const expiry = page.locator("#expires-on");
    const create = page.getByRole("button", { name: "Create token", exact: true });

    await expiry.fill(isoDaysFromNow(400));

    // Out of range clears the field's value while leaving the text on screen,
    // so the only signals are the invalid state and a create that stays shut.
    await expect(expiry).toHaveAttribute("aria-invalid", "true");
    await expect(create).toBeDisabled();

    // Every other field is already filled, so a date inside the cap enabling
    // Create is what proves the date was the reason it was disabled.
    await expiry.fill(isoDaysFromNow(300));

    await expect(expiry).not.toHaveAttribute("aria-invalid", "true");
    await expect(create).toBeEnabled();
  });

  // Tokens are self-service: holding no administrative grant does not stop
  // someone issuing a credential for the repositories they can already read.
  test("a reader creates a token of its own", async ({ signedInAs, adminApi }) => {
    const name = scopedName("tok");
    await deleteTokenIfPresent(adminApi, "reader", name);
    const page = await signedInAs("reader");

    try {
      await page.goto("/workspace/tokens");

      await expect(page.getByTestId("page-tokens")).toBeVisible();
      await expect(page.getByRole("button", { name: "New token" })).toBeEnabled();

      await createTokenThroughUI(page, name, "*");

      await expect(page.getByTestId(`row-${name}`)).toBeVisible();
    } finally {
      await page.close();
      await deleteTokenIfPresent(adminApi, "reader", name);
    }
  });
});

// Fills the create form and dismisses the secret panel, leaving the caller on
// the token list. The secret is asserted on the way past: a helper that swallows
// it would let a test pass against a create that returned nothing.
async function createTokenThroughUI(page: Page, name: string, pattern: string) {
  await page.goto("/workspace/tokens/new");
  await page.locator("#token-name").fill(name);
  await page.locator("#token-description").fill("e2e");
  await page.locator("#expires-on").fill(isoDaysFromNow(30));
  await typeRepositoryPattern(page, pattern);
  await page.getByRole("button", { name: "Add permission" }).click();
  await page.getByRole("button", { name: "Create token", exact: true }).click();

  await expect(page.getByTestId("page-token-created").getByText(SECRET)).toBeVisible();
  await page.getByRole("button", { name: "Done" }).click();
}

// Tokens belonging to somebody else are only reachable through the admin
// endpoints, which is what the fixture's admin context is for: the signed-in
// role can see its own tokens, but a test cannot clean up after a failure that
// left the browser somewhere else.
async function deleteTokenIfPresent(adminApi: APIRequestContext, role: E2ERole, name: string) {
  const userId = await userIdFor(adminApi, role);
  const response = await adminApi.get(`/api/v1/users/${userId}/tokens`);
  const tokens = (await response.json()) as { id: number; name: string }[];
  const token = tokens.find((candidate) => candidate.name === name);

  if (token) await adminApi.delete(`/api/v1/users/${userId}/tokens/${token.id}`);
}

async function userIdFor(adminApi: APIRequestContext, role: E2ERole): Promise<number> {
  const { username } = accountFor(role);
  const response = await adminApi.get("/api/v1/users");
  const users = (await response.json()) as { id: number; username: string }[];
  const user = users.find((candidate) => candidate.username === username);

  if (!user) throw new Error(`e2e account ${username} is missing; run \`make e2e\``);

  return user.id;
}

// The field takes a local YYYY-MM-DD, so the date is built in local time rather
// than sliced off an ISO string: near midnight a UTC slice is the wrong day,
// and one day either side of the one-year cap is the whole point of these.
function isoDaysFromNow(days: number): string {
  const date = new Date();
  date.setDate(date.getDate() + days);
  const pad = (value: number) => String(value).padStart(2, "0");

  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}
