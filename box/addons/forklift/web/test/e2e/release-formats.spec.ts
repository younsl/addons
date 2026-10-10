import { createHash } from "node:crypto";

import { expect, scopedName, test } from "./setup/fixtures";
import { packageFixture, type PackageFormat } from "./setup/packages";

const formats: PackageFormat[] = ["maven", "npm", "cargo", "go", "pypi", "raw"];

for (const format of formats) {
  test(`${format}: upload, byte-exact download, multi-select labels and lifecycle`, async ({ signedInAs, adminApi }) => {
    test.setTimeout(90_000);
    const name = `${scopedName(`release-${format}`)}-${Date.now().toString(36)}`;
    const created = await adminApi.post("/api/v1/repositories", {
      data: { name, format, type: "hosted", upstream_url: "", config: { age_policy: { enabled: false } } },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const { id } = await created.json();
    const me = await (await adminApi.get("/api/v1/me")).json();
    const csrfHeaders = { "X-CSRF-Token": me.csrf_token };
    const files = ["1.0.0", "1.0.1"].map((version) => packageFixture(format, version));
    const page = await signedInAs("admin");
    try {
      for (const file of files) {
        await page.goto(`/workspace/repositories/${id}/upload`);
        await page.locator('input[type="file"]').setInputFiles({ name: file.name, mimeType: file.mimeType, buffer: file.buffer });
        if (format === "maven") {
          await page.locator("#upload-group-id").fill("com.example");
          await page.locator("#upload-artifact-id").fill("releaseprobe");
          await page.locator("#upload-version").fill(file.version);
        }
        if (format === "go") {
          await page.locator("#upload-go-module").fill(file.module);
          await page.locator("#upload-go-version").fill(`v${file.version}`);
        }
        await page.getByRole("button", { name: /upload/i }).click();
        await expect(page.getByText(/upload complete|publication committed/i)).toBeVisible();
        const download = await adminApi.get(`/${format}/${name}/${file.path}`);
        expect(download.status(), await download.text()).toBe(200);
        expect(await download.body()).toEqual(file.buffer);
      }

      await page.goto(`/workspace/repositories/${id}/artifacts`);
      const label = "release:verified";
      for (const action of ["Add", "Remove"] as const) {
        for (const file of files) await page.getByRole("checkbox", { name: `Select ${file.path}`, exact: true }).check();
        await page.getByRole("button", { name: "Actions", exact: true }).click();
        await page.getByRole("button", { name: `${action} label`, exact: true }).last().click();
        const dialog = page.getByRole("alertdialog");
        await dialog.getByRole("textbox").fill(label);
        const done = page.waitForResponse((response) => response.url().endsWith(`/repositories/${id}/artifacts/labels/bulk`) && response.request().method() === "POST");
        await dialog.getByRole("button", { name: action, exact: true }).click();
        expect((await done).ok()).toBe(true);
        await expect(dialog).toBeHidden();
        for (const file of files) {
          const checkbox = page.getByRole("checkbox", { name: `Select ${file.path}`, exact: true });
          await expect(checkbox).not.toBeChecked();
          await expect(checkbox).toBeEnabled();
        }
        await expect.poll(async () => {
          const response = await adminApi.get(`/api/v1/repositories/${id}/artifacts?limit=100`);
          const { artifacts } = await response.json();
          return files.map((file) => artifacts.find((a: { path: string }) => a.path === file.path)?.labels?.some((l: { label: string }) => l.label === label) ?? false);
        }).toEqual([action === "Add", action === "Add"]);
      }

      if (format === "raw") {
        for (const file of files) await page.getByRole("checkbox", { name: `Select ${file.path}`, exact: true }).check();
        await page.getByRole("button", { name: "Actions", exact: true }).click();
        await page.getByRole("button", { name: "Delete", exact: true }).click();
        const dialog = page.getByRole("alertdialog");
        await dialog.getByRole("textbox").fill("delete");
        await dialog.getByRole("button", { name: "Delete", exact: true }).click();
        for (const file of files) await expect.poll(async () => (await adminApi.get(`/${format}/${name}/${file.path}`)).status()).toBe(404);
      } else {
        const response = await adminApi.get(`/api/v1/repositories/${id}/artifacts?limit=100`);
        expect(response.ok()).toBe(true);
        const body = await response.json();
        const publications = Array.isArray(body) ? body : body.publications;
        expect(publications).toHaveLength(2);
        for (const publication of publications) {
          if (format === "cargo") {
            const yank = await adminApi.post(`/api/v1/repositories/${id}/publications/${publication.id}/yank`, { headers: csrfHeaders, data: { yanked: true } });
            expect(yank.ok(), await yank.text()).toBe(true);
          } else {
            const removed = await adminApi.delete(`/api/v1/repositories/${id}/publications/${publication.id}`, { headers: csrfHeaders });
            expect(removed.status(), await removed.text()).toBe(format === "go" ? 409 : 200);
          }
        }
        for (const file of files) {
          const expected = format === "cargo" || format === "go" ? 200 : 404;
          await expect.poll(async () => (await adminApi.get(`/${format}/${name}/${file.path}`)).status()).toBe(expected);
        }
      }
    } finally {
      await page.close();
      const removed = await adminApi.delete(`/api/v1/repositories/${id}`);
      expect(removed.ok(), await removed.text()).toBe(true);
    }
  });
}

// OCI uses native registry publication; its stored manifests still participate
// in the same browser selection and labeling workflow as package artifacts.
test("oci: native push/download/delete and multi-select labels", async ({ signedInAs, adminApi }) => {
  const name = `${scopedName("release-oci")}-${Date.now().toString(36)}`;
  const created = await adminApi.post("/api/v1/repositories", {
    data: { name, format: "oci", type: "hosted", upstream_url: "", config: { age_policy: { enabled: false } } },
  });
  expect(created.ok(), await created.text()).toBe(true);
  const { id } = await created.json();
  const digest = (bytes: Buffer) => `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  const manifests: { digest: string; path: string; version: string; bytes: Buffer }[] = [];
  const page = await signedInAs("admin");
  try {
    for (const version of ["1.0.0", "1.0.1"]) {
      const config = Buffer.from(JSON.stringify({ architecture: "amd64", os: "linux", config: { Labels: { version } } }));
      const pushed = await adminApi.post(`/v2/${name}/releaseprobe/blobs/uploads/?digest=${digest(config)}`, {
        data: config, headers: { "content-type": "application/octet-stream" },
      });
      expect(pushed.status(), await pushed.text()).toBe(201);
      const bytes = Buffer.from(JSON.stringify({
        schemaVersion: 2, mediaType: "application/vnd.oci.image.manifest.v1+json",
        config: { mediaType: "application/vnd.oci.image.config.v1+json", digest: digest(config), size: config.length }, layers: [],
      }));
      const published = await adminApi.put(`/v2/${name}/releaseprobe/manifests/${version}`, {
        data: bytes, headers: { "content-type": "application/vnd.oci.image.manifest.v1+json" },
      });
      expect(published.status(), await published.text()).toBe(201);
      const manifestDigest = digest(bytes);
      manifests.push({ digest: manifestDigest, path: `releaseprobe/manifests/${manifestDigest}`, version, bytes });
      const download = await adminApi.get(`/v2/${name}/releaseprobe/manifests/${version}`);
      expect(download.status()).toBe(200);
      expect(await download.body()).toEqual(bytes);
      const blob = await adminApi.get(`/v2/${name}/releaseprobe/blobs/${digest(config)}`);
      expect(blob.status()).toBe(200);
      expect(await blob.body()).toEqual(config);
    }
    await page.goto(`/workspace/repositories/${id}/artifacts`);
    for (const action of ["Add", "Remove"] as const) {
      for (const manifest of manifests) await page.getByRole("checkbox", { name: `Select releaseprobe:${manifest.version}`, exact: true }).check();
      await page.getByRole("button", { name: "Actions", exact: true }).click();
      await page.getByRole("button", { name: `${action} label`, exact: true }).last().click();
      const dialog = page.getByRole("alertdialog");
      await dialog.getByRole("textbox").fill("release:verified");
      const done = page.waitForResponse((response) => response.url().endsWith(`/repositories/${id}/artifacts/labels/bulk`) && response.request().method() === "POST");
      await dialog.getByRole("button", { name: action, exact: true }).click();
      expect((await done).ok()).toBe(true);
      await expect(dialog).toBeHidden();
      for (const manifest of manifests) {
        const checkbox = page.getByRole("checkbox", { name: `Select releaseprobe:${manifest.version}`, exact: true });
        await expect(checkbox).not.toBeChecked();
        await expect(checkbox).toBeEnabled();
      }
      const response = await adminApi.get(`/api/v1/repositories/${id}/artifacts?limit=100`);
      const { artifacts } = await response.json();
      for (const manifest of manifests) {
        const artifact = artifacts.find((row: { path: string }) => row.path === manifest.path);
        expect(artifact).toBeDefined();
        expect(artifact.labels?.some((label: { label: string }) => label.label === "release:verified") ?? false).toBe(action === "Add");
      }
    }
    const reader = await signedInAs("reader");
    await reader.goto(`/workspace/repositories/${id}/artifacts`);
    for (const manifest of manifests) {
      await expect(reader.getByRole("checkbox", { name: `Select releaseprobe:${manifest.version}`, exact: true })).toBeDisabled();
    }
    await expect(reader.getByRole("button", { name: "Actions", exact: true })).toBeDisabled();
    await reader.close();
    for (const manifest of manifests) {
      const removed = await adminApi.delete(`/v2/${name}/releaseprobe/manifests/${manifest.digest}`);
      expect(removed.status(), await removed.text()).toBe(202);
      expect((await adminApi.get(`/v2/${name}/releaseprobe/manifests/${manifest.digest}`)).status()).toBe(404);
    }
  } finally {
    await page.close();
    await expect(await adminApi.delete(`/api/v1/repositories/${id}`)).toBeOK();
  }
});
