import { expect, scopedName, test } from "./setup/fixtures";

import type { APIRequestContext, Page } from "@playwright/test";

// Publishing an artifact through the browser.
//
// Fixtures are generated here rather than committed. A real jar or wheel would
// put binaries in the repository for the sake of a test, and the bytes do not
// matter to what is being checked: the client only reads an archive to guess
// coordinates, and says so - archive.ts resolves any malformed input to null
// and the form falls back to the filename. That fallback is itself worth
// pinning, because it is what a user hits with any file the parser cannot read.
//
// Raw is the format used for the end-to-end round trip: it has no coordinate
// parsing at all, so an upload either works or does not, with nothing in
// between to explain a failure away.

test.describe("raw upload", () => {
  test("a file uploaded through the form appears in the artifacts tab", async ({
    signedInAs,
    adminApi,
  }) => {
    const repository = scopedName("raw");
    const fileName = `${scopedName("blob")}.bin`;
    const id = await createRepository(adminApi, { name: repository, format: "raw" });

    const page = await signedInAs("admin");
    try {
      await page.goto(`/workspace/repositories/${id}/upload`);

      await attachFile(page, fileName, "e2e payload");
      await page.getByRole("button", { name: /upload/i }).click();
      // Navigating before the upload settles cancels it, and the artifacts tab
      // would then be empty for a reason that has nothing to do with the tab.
      await expect(page.getByText(/upload complete/i)).toBeVisible();

      // The artifacts tab is the acknowledgement that matters: the form saying
      // "done" only means the request returned.
      await page.goto(`/workspace/repositories/${id}/artifacts`);
      await expect(page.getByText(fileName)).toBeVisible();
    } finally {
      await page.close();
      await adminApi.delete(`/api/v1/repositories/${id}`);
    }
  });

  test("uploading the same path twice is refused rather than silently overwriting", async ({
    signedInAs,
    adminApi,
  }) => {
    const repository = scopedName("dup");
    const fileName = `${scopedName("blob")}.bin`;
    const id = await createRepository(adminApi, { name: repository, format: "raw" });

    const page = await signedInAs("admin");
    try {

      for (const attempt of [1, 2]) {
        await page.goto(`/workspace/repositories/${id}/upload`);
        await attachFile(page, fileName, "e2e payload");
        await page.getByRole("button", { name: /upload/i }).click();

        if (attempt === 1) {
          // Waited for, not assumed. The click only dispatches; the form then
          // makes two round trips, and navigating away cancels whichever is in
          // flight - which aborts the write server-side and leaves nothing for
          // the second attempt to collide with. The success card is the only
          // signal that the artifact actually landed.
          await expect(page.getByText(/upload complete/i)).toBeVisible();
        }

        if (attempt === 2) {
          // An overwrite would lose whatever the first upload published, so the
          // server refuses and the form says so instead of appearing to work.
          await expect(page.getByText(/already exists/i)).toBeVisible();
        }
      }
    } finally {
      await page.close();
      await adminApi.delete(`/api/v1/repositories/${id}`);
    }
  });
});

// Each ecosystem gets its own form, and the route picks between them by format.
// A wrong pick is invisible until someone tries to publish, so this checks the
// form that appears is the one that belongs to the repository.
test.describe("each format gets its own form", () => {
  // Maven is the one form that shows nothing about coordinates until a file is
  // attached - the whole card is behind `hasFile`, because the coordinates are
  // read out of the archive. So its field only exists after an upload is
  // chosen, and asking for it on an empty form finds nothing.
  const FORMATS = [
    { format: "maven", field: /group ?id/i, needsFile: true },
    { format: "npm", field: /tarball|package/i, needsFile: false },
    { format: "pypi", field: /distribution|file/i, needsFile: false },
    { format: "cargo", field: /crate|file/i, needsFile: false },
    { format: "go", field: /module/i, needsFile: false },
  ];

  for (const { format, field, needsFile } of FORMATS) {
    test(`${format} publishes through its own fields`, async ({ signedInAs, adminApi }) => {
      const id = await createRepository(adminApi, {
        name: scopedName(format),
        format,
      });

      const page = await signedInAs("admin");
      try {
        await page.goto(`/workspace/repositories/${id}/upload`);

        await expect(page.getByRole("heading", { name: /upload/i })).toBeVisible();
        if (needsFile) await attachFile(page, `probe-1.0.0.jar`, "e2e payload");
        await expect(page.getByText(field).first()).toBeVisible();
      } finally {
        await page.close();
        await adminApi.delete(`/api/v1/repositories/${id}`);
      }
    });
  }

  // The Maven form fills the coordinates from the filename when it cannot read
  // the archive, which is every archive this suite generates - and, in
  // practice, anything the browser's zip reader chokes on.
  test("maven falls back to the filename when the archive cannot be read", async ({
    signedInAs,
    adminApi,
  }) => {
    const id = await createRepository(adminApi, {
      name: scopedName("mvn"),
      format: "maven",
    });

    const page = await signedInAs("admin");
    try {
      await page.goto(`/workspace/repositories/${id}/upload`);

      await attachFile(page, "mylib-1.2.3.jar", "not really a jar");

      // groupId is not recoverable from a filename, so it stays for the user.
      await expect(page.locator("#upload-artifact-id")).toHaveValue("mylib");
      await expect(page.locator("#upload-version")).toHaveValue("1.2.3");
    } finally {
      await page.close();
      await adminApi.delete(`/api/v1/repositories/${id}`);
    }
  });
});

// A repository a test owns outright, so an upload cannot disturb a seeded one
// that other workers are reading.
async function createRepository(
  adminApi: APIRequestContext,
  { name, format }: { name: string; format: string },
): Promise<number> {
  const response = await adminApi.post("/api/v1/repositories", {
    data: {
      name,
      format,
      type: "hosted",
      upstream_url: "",
      config: {
        cache: { enabled: true, metadata_ttl: "15m", negative_ttl: "5m", eviction: "lru" },
        age_policy: { enabled: false },
        policy_pipeline: { schema_version: 2, order: ["vulnerability", "license", "age"] },
      },
    },
  });

  if (!response.ok()) {
    throw new Error(`e2e: could not create ${format} repository (${response.status()})`);
  }

  return (await response.json()).id;
}

async function attachFile(page: Page, name: string, contents: string) {
  await page.locator('input[type="file"]').setInputFiles({
    name,
    mimeType: "application/octet-stream",
    buffer: Buffer.from(contents),
  });
}
