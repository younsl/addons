import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import { ApiError } from "@/lib/http/error/api-error";
// The generator groups GET /storage into the ha module alongside GET /ha;
// both are single-segment system endpoints. Mock where it is generated.
import * as haApi from "@/services/v1/ha/api";
import { useStorageStatus } from "@/routes/admin/-storage/hooks/use-storage-status";

vi.mock("@/services/v1/ha/api");

const mockedHa = vi.mocked(haApi);

const storage = {
  backend: "fs" as const,
  mode: "filesystem" as const,
  blob_count: 12,
  blob_bytes: 4096,
  dangling: [],
};

function withQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });

  return {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

beforeEach(() => {
  mockedHa.getStorage.mockResolvedValue(storage);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useStorageStatus", () => {
  test("polls while auto-refresh is on", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useStorageStatus(), { wrapper });

    await waitFor(() => expect(mockedHa.getStorage).toHaveBeenCalledTimes(1));
    expect(result.current.isAutoRefreshing).toBe(true);

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });

    expect(mockedHa.getStorage).toHaveBeenCalledTimes(2);
  });

  test("turning the toggle off stops the polling", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useStorageStatus(), { wrapper });

    await waitFor(() => expect(mockedHa.getStorage).toHaveBeenCalledTimes(1));
    act(() => result.current.setAutoRefreshing(false));

    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });

    expect(mockedHa.getStorage).toHaveBeenCalledTimes(1);
  });

  // "Last updated" has to mean when the data arrived, not when a refresh was
  // attempted - otherwise a failing backend shows a ticking clock over stale
  // numbers, which reads as healthy.
  test("a failed refresh leaves the timestamp at the last good fetch", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useStorageStatus(), { wrapper });

    await waitFor(() => expect(result.current.storage).toEqual(storage));
    const firstUpdate = result.current.updatedAt;
    expect(firstUpdate).not.toBeNull();

    mockedHa.getStorage.mockRejectedValue(new ApiError(503, "storage unavailable"));
    await act(async () => { await result.current.refresh(); });

    await waitFor(() => expect(result.current.error).toBe("storage unavailable"));
    expect(result.current.updatedAt?.getTime()).toBe(firstUpdate?.getTime());
    // The last good data is still on screen behind the error, rather than the
    // page emptying out.
    expect(result.current.storage).toEqual(storage);
  });
});
