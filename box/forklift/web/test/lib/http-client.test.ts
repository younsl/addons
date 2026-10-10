import { afterEach, describe, expect, it, vi } from "vitest";

import { createHttpClient } from "@/lib/http/client/http-client";
import { ApiError } from "@/lib/http/error/api-error";

function respondWith(status: number, body: string, statusText = "") {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(body, { status, statusText })),
  );
}

afterEach(() => vi.unstubAllGlobals());

// The screens read err.message in 52 places. Raising ApiError instead of Error
// must not change what that string says, or every one of them shifts at once.
describe("httpClient error messages", () => {
  it("keeps using the error field the server handlers send", async () => {
    respondWith(409, JSON.stringify({ error: "repository name already exists" }));

    await expect(createHttpClient().get("/x")).rejects.toThrow(
      "repository name already exists",
    );
  });

  it("falls back to the raw body when middleware answers in plain text", async () => {
    respondWith(403, "forbidden");

    await expect(createHttpClient().get("/x")).rejects.toThrow("forbidden");
  });

  it("falls back to statusText when there is no body at all", async () => {
    respondWith(500, "", "Internal Server Error");

    await expect(createHttpClient().get("/x")).rejects.toThrow(
      "Internal Server Error",
    );
  });
});

describe("httpClient error shape", () => {
  it("now carries the status and field errors, which a string could not", async () => {
    respondWith(
      400,
      JSON.stringify({ error: "invalid", field_errors: { name: ["required"] } }),
    );

    const error = await createHttpClient()
      .get("/x")
      .catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(400);
    expect((error as ApiError).fieldErrors).toEqual({ name: ["required"] });
  });

  it("marks a request that never reached the server as status 0", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => { throw new TypeError("Failed to fetch"); }));

    const error = await createHttpClient()
      .get("/x")
      .catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(0);
    expect((error as ApiError).code).toBe("NETWORK_ERROR");
  });

  it("lets an abort through as a DOMException rather than wrapping it", async () => {
    const controller = new AbortController();
    vi.stubGlobal("fetch", vi.fn(async () => {
      controller.abort();
      throw new DOMException("aborted", "AbortError");
    }));

    const error = await createHttpClient()
      .get("/x", { signal: controller.signal })
      .catch((caught: unknown) => caught);

    // Not toBeInstanceOf: jsdom and node each have their own DOMException, so
    // the check would fail across realms even though the value is right.
    expect(error).not.toBeInstanceOf(ApiError);
    expect((error as DOMException).name).toBe("AbortError");
  });

  it("returns undefined for 204 without trying to parse a body", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(null, { status: 204 })));

    await expect(createHttpClient().delete("/x")).resolves.toBeUndefined();
  });
});
