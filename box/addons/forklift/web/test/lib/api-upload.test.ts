import { afterEach, describe, expect, it, vi } from "vitest";

import { ArtifactUploadResult, uploadArtifact } from "@/api";

class FakeXMLHttpRequest {
  static latest: FakeXMLHttpRequest;
  method = "";
  url = "";
  withCredentials = false;
  timeout = 0;
  status = 0;
  statusText = "";
  responseText = "";
  headers = new Map<string, string>();
  upload: { onprogress: ((event: ProgressEvent) => void) | null } = { onprogress: null };
  onerror: (() => void) | null = null;
  ontimeout: (() => void) | null = null;
  onabort: (() => void) | null = null;
  onload: (() => void) | null = null;
  onloadend: (() => void) | null = null;
  sent: Document | XMLHttpRequestBodyInit | null = null;

  constructor() {
    FakeXMLHttpRequest.latest = this;
  }

  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }

  setRequestHeader(name: string, value: string) {
    this.headers.set(name, value);
  }

  send(body: Document | XMLHttpRequestBodyInit | null) {
    this.sent = body;
  }

  abort() {
    this.onabort?.();
    this.onloadend?.();
  }
}

describe("uploadArtifact", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends credentials, idempotency and CSRF headers and reports progress", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXMLHttpRequest);
    const onProgress = vi.fn();
    const promise = uploadArtifact(17, new FormData(), {
      idempotencyKey: "request-key",
      csrfToken: "csrf-value",
      onProgress,
    });
    const xhr = FakeXMLHttpRequest.latest;
    expect(xhr.method).toBe("POST");
    expect(xhr.url).toBe("/api/v1/repositories/17/uploads");
    expect(xhr.withCredentials).toBe(true);
    expect(xhr.headers.get("Idempotency-Key")).toBe("request-key");
    expect(xhr.headers.get("X-CSRF-Token")).toBe("csrf-value");

    xhr.upload.onprogress?.({ loaded: 5, total: 10, lengthComputable: true } as ProgressEvent);
    expect(onProgress).toHaveBeenCalledWith(5, 10);

    const result: ArtifactUploadResult = {
      upload_id: "upload-1", repository: "maven-local", format: "maven",
      coordinate: "com.acme:widget:1.0.0", created: [], replaced: [], derived: [],
      scan_status: "queued", durability: "local", warnings: [],
    };
    xhr.status = 201;
    xhr.responseText = JSON.stringify(result);
    xhr.onload?.();
    await expect(promise).resolves.toEqual(result);
  });

  it("preserves structured upload problems", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXMLHttpRequest);
    const promise = uploadArtifact(17, new FormData(), { idempotencyKey: "request-key" });
    const xhr = FakeXMLHttpRequest.latest;
    xhr.status = 409;
    xhr.responseText = JSON.stringify({
      type: "https://forklift.dev/problems/artifact-conflict",
      title: "Artifact already exists",
      status: 409,
      code: "artifact_conflict",
      detail: "The target path exists",
      conflicts: ["com/acme/widget/1.0.0/widget-1.0.0.jar"],
    });
    xhr.onload?.();
    await expect(promise).rejects.toMatchObject({
      name: "UploadAPIError",
      problem: { code: "artifact_conflict", status: 409 },
    });
  });

  it("aborts the request through the caller signal", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXMLHttpRequest);
    const controller = new AbortController();
    const promise = uploadArtifact(17, new FormData(), {
      idempotencyKey: "request-key",
      signal: controller.signal,
    });
    controller.abort();
    await expect(promise).rejects.toMatchObject({ name: "AbortError" });
  });
});
