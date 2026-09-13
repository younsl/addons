import type { APIRequestContext } from "@playwright/test";

import { expect, scopedName, test } from "./setup/fixtures";

// Notification receivers: the round trip, plus the one thing about this screen
// that keeps being "fixed" back into a bug.
//
// The webhook URL is write-only. The API never returns it, so the edit form
// opens with that field blank on a receiver that demonstrably has one saved.
// That reads as a load failure, and prefilling it - with the URL, with a mask,
// with anything - either leaks the secret or silently overwrites it with the
// mask on the next save. The second test pins the blank field so the next
// person to "fix" it hears about it here.

const WEBHOOK = "https://hooks.example.invalid/services/e2e";

test.describe("notification receivers", () => {
  test("a receiver is created, edited and deleted", async ({ signedInAs, adminApi }) => {
    const name = scopedName("rcv");
    const page = await signedInAs("admin");
    // Cleared before the test as well as after it. A run that dies partway
    // leaves its receiver behind, and every later run would then collide with
    // it - which is a confusing way to be told about an old failure.
    await deleteReceiverIfPresent(adminApi, name);

    try {
      await page.goto("/admin/notifications");
      await page.getByRole("button", { name: "Add receiver" }).click();

      await expect(page.getByTestId("page-receiver-form")).toBeVisible();
      await page.locator("#rcv-name").fill(name);
      await page.locator("#rcv-desc").fill("e2e round trip");
      await page.locator("#rcv-url").fill(WEBHOOK);
      await page.getByRole("button", { name: "Add receiver" }).click();

      await expect(page).toHaveURL(/\/admin\/notifications\/?$/);
      const row = page.getByTestId(`row-${name}`);
      await expect(row).toBeVisible();
      // All the list can say about a write-only URL is that one is set.
      await expect(row).toContainText("configured");

      await row.getByRole("button", { name: "Edit" }).click();
      await expect(page.getByTestId("page-receiver-form")).toBeVisible();
      await page.locator("#rcv-desc").fill("e2e edited");
      await page.getByRole("button", { name: "Save changes" }).click();

      await expect(page).toHaveURL(/\/admin\/notifications\/?$/);
      await expect(page.getByTestId(`row-${name}`)).toContainText("e2e edited");
      // Saving with the URL field left blank keeps the stored one rather than
      // clearing it, which is the other half of the write-only contract.
      await expect(page.getByTestId(`row-${name}`)).toContainText("configured");

      await page.getByTestId(`row-${name}`).getByRole("button", { name: "Edit" }).click();
      await page.getByRole("button", { name: "Delete", exact: true }).click();
      const confirm = page.getByRole("alertdialog");
      // Type-to-confirm: deleting a receiver silences whatever still points at
      // it, which is not visible from this page.
      await confirm.getByRole("textbox").fill(name);
      await confirm.getByRole("button", { name: "Delete", exact: true }).click();

      await expect(page).toHaveURL(/\/admin\/notifications\/?$/);
      await expect(page.getByTestId(`row-${name}`)).toBeHidden();
    } finally {
      await page.close();
      // The delete is the last assertion, so an earlier failure would leave the
      // receiver behind for the next run.
      await deleteReceiverIfPresent(adminApi, name);
    }
  });

  // The regression this file exists for. A blank field here is correct, not a
  // failed load - which the populated name field alongside it proves.
  test("the webhook field opens blank on a receiver that has one saved", async ({
    signedInAs,
    adminApi,
  }) => {
    const name = scopedName("rcv");
    const receiverId = await createReceiver(adminApi, name);
    const page = await signedInAs("admin");

    try {
      await page.goto("/admin/notifications");
      // Asserted before opening the form: without this the empty field below
      // would also pass against a receiver that never had a URL at all.
      await expect(page.getByTestId(`row-${name}`)).toContainText("configured");

      await page.goto(`/admin/notifications/${receiverId}`);

      await expect(page.getByTestId("page-receiver-form")).toBeVisible();
      // The rest of the receiver did load, so nothing is missing except the URL.
      await expect(page.locator("#rcv-name")).toHaveValue(name);
      await expect(page.locator("#rcv-url")).toHaveValue("");
    } finally {
      await page.close();
      await deleteReceiverIfPresent(adminApi, name);
    }
  });

  // On an edit a blank field means "test the stored URL", but a receiver being
  // created has no stored URL to fall back on, so the form has to say so
  // itself rather than posting an empty webhook to the server.
  test("testing a new receiver with no webhook reports an error locally", async ({
    signedInAs,
  }) => {
    const page = await signedInAs("admin");
    const testRequests: string[] = [];
    page.on("request", (request) => {
      if (request.url().includes("/api/v1/notification/test")) testRequests.push(request.url());
    });

    await page.goto("/admin/notifications/new");
    await page.locator("#rcv-name").fill(scopedName("rcv"));
    await page.getByRole("button", { name: "Send test" }).click();

    await expect(page.getByText("Enter a webhook URL first.")).toBeVisible();
    expect(testRequests).toEqual([]);
  });
});

async function createReceiver(adminApi: APIRequestContext, name: string): Promise<number> {
  await deleteReceiverIfPresent(adminApi, name);
  const response = await adminApi.post("/api/v1/notification/receivers", {
    data: { name, description: "e2e fixture", webhook_url: WEBHOOK, enabled: true },
  });
  expect(response.ok()).toBe(true);

  return (await response.json()).id as number;
}

async function deleteReceiverIfPresent(adminApi: APIRequestContext, name: string) {
  const response = await adminApi.get("/api/v1/notification/receivers");
  const receivers = (await response.json()) as { id: number; name: string }[];
  const receiver = receivers.find((candidate) => candidate.name === name);

  if (receiver) await adminApi.delete(`/api/v1/notification/receivers/${receiver.id}`);
}
