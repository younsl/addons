import { expect, test } from "./setup/fixtures";

// Manual failover exists only in a clustered deployment. The test server runs a
// single instance, so the control must be absent rather than present and
// broken - a step-down button that cannot work is worse than none.
test("the step-down control is absent on a single instance", async ({ signedInAs }) => {
  const page = await signedInAs("admin");
  await page.goto("/admin/ha");

  await expect(page.getByTestId("page-ha")).toBeVisible();
  await expect(page.getByTestId("panel-danger-zone")).toBeHidden();
});
