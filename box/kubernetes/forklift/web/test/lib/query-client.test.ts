import { describe, expect, it, vi } from "vitest";

import { ApiError } from "@/lib/http/error/api-error";
import { createQueryClient } from "@/lib/react-query/query-client";

// The client is wired but has no global handler yet. These pin the routing
// rules so that adding one later cannot quietly change who reports what.
async function runFailingQuery(
  client: ReturnType<typeof createQueryClient>,
  error: unknown,
  meta?: Record<string, unknown>,
) {
  await client
    .fetchQuery({
      queryKey: ["failing", Math.random()],
      queryFn: () => Promise.reject(error),
      retry: false,
      meta,
    })
    .catch(() => undefined);
}

describe("createQueryClient error routing", () => {
  it("does not report a cancelled request", async () => {
    const onQueryError = vi.fn();
    const client = createQueryClient({ onQueryError });

    await runFailingQuery(client, new DOMException("aborted", "AbortError"));

    expect(onQueryError).not.toHaveBeenCalled();
  });

  it("reports an ordinary failure to the global handler", async () => {
    const onQueryError = vi.fn();
    const client = createQueryClient({ onQueryError });

    await runFailingQuery(client, new Error("boom"));

    expect(onQueryError).toHaveBeenCalledOnce();
  });

  it("lets a query claim the error for itself", async () => {
    const onQueryError = vi.fn();
    const errorHandler = vi.fn();
    const client = createQueryClient({ onQueryError });

    await runFailingQuery(client, new Error("boom"), { errorHandler });

    expect(errorHandler).toHaveBeenCalledOnce();
    expect(onQueryError).not.toHaveBeenCalled();
  });

  it("lets a query opt out of the global handler", async () => {
    const onQueryError = vi.fn();
    const client = createQueryClient({ onQueryError });

    await runFailingQuery(client, new Error("boom"), {
      suppressGlobalErrorToast: true,
    });

    expect(onQueryError).not.toHaveBeenCalled();
  });

  it("survives having no handler at all, which is today's wiring", async () => {
    const client = createQueryClient();

    await expect(
      runFailingQuery(client, new Error("boom")),
    ).resolves.toBeUndefined();
  });
});

// A 4xx is an answer, not a hiccup. The client used to retry everything once,
// which was harmless while two queries used it and wasteful once 39 did -
// several of them 403 by design, so every one of those cost two requests and
// took twice as long to say so.
describe("retry policy", () => {
  const retry = (error: unknown, failureCount = 0) => {
    const client = createQueryClient();
    const configured = client.getDefaultOptions().queries?.retry;

    return typeof configured === "function"
      ? configured(failureCount, error as Error)
      : configured;
  };

  it.each([
    ["not signed in", 401],
    ["not allowed", 403],
    ["not there", 404],
    ["conflict", 409],
  ])("does not retry a %s", (_name, status) => {
    expect(retry(new ApiError(status, "no"))).toBe(false);
  });

  it("retries a server error once", () => {
    expect(retry(new ApiError(503, "unavailable"), 0)).toBe(true);
    expect(retry(new ApiError(503, "unavailable"), 1)).toBe(false);
  });

  // httpClient raises status 0 when the request never reached a response.
  it("retries a transport failure once", () => {
    expect(retry(new ApiError(0, "Network request failed"), 0)).toBe(true);
  });

  it("retries an unrecognised failure once", () => {
    expect(retry(new Error("boom"), 0)).toBe(true);
  });
});
