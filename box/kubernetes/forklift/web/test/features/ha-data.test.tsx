import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import * as haApi from "@/services/v1/ha/api";
import { useHaStatus } from "@/routes/admin/-ha/hooks/use-ha-status";
import { formatUptime, storageUsage } from "@/routes/admin/-ha/utils/ha-status";

vi.mock("@/services/v1/ha/api");

const mockedHa = vi.mocked(haApi);

const status = {
  mode: "ha",
  backend: "s3",
  enabled: true,
  is_leader: true,
  identity: "forklift-0",
  leader: "forklift-0",
  role: "leader",
};

function withQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

  return {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

beforeEach(() => {
  mockedHa.getHa.mockResolvedValue(status as never);
  mockedHa.getStorage.mockResolvedValue({
    backend: "fs",
    mode: "filesystem",
    blob_count: 0,
    blob_bytes: 0,
    dangling: [],
  });
  mockedHa.postStepDownHa.mockResolvedValue(undefined as never);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useHaStatus", () => {
  // Leadership and capacity change on completely different timescales: a
  // failover is seconds, a volume filling is days. Polling them together would
  // put a MinIO Admin API round trip on the 5s election beat.
  test("polls leadership fast and storage slowly", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { wrapper } = withQueryClient();
    renderHook(() => useHaStatus(), { wrapper });

    await waitFor(() => expect(mockedHa.getHa).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(mockedHa.getStorage).toHaveBeenCalledTimes(1));

    await act(async () => { await vi.advanceTimersByTimeAsync(15_000); });

    // Three more 5s beats for the status; the 30s storage poll has not come round.
    expect(mockedHa.getHa).toHaveBeenCalledTimes(4);
    expect(mockedHa.getStorage).toHaveBeenCalledTimes(1);
  });

  test("the countdown runs down to the next poll", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useHaStatus(), { wrapper });

    await waitFor(() => expect(result.current.status).toBeDefined());
    const initial = result.current.secondsLeft;
    expect(initial).toBeGreaterThan(0);
    expect(initial).toBeLessThanOrEqual(5);

    await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });

    expect(result.current.secondsLeft).toBeLessThan(initial);
  });

  test("stepping down refetches at once rather than waiting out the poll", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useHaStatus(), { wrapper });

    await waitFor(() => expect(mockedHa.getHa).toHaveBeenCalledTimes(1));
    await act(async () => { result.current.stepDown(); });

    expect(mockedHa.postStepDownHa).toHaveBeenCalled();
    // The leader is changing underneath; the table has to swap roles now, not
    // in up to five seconds.
    await waitFor(() => expect(mockedHa.getHa).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.notice).not.toBe(""));
  });

  // The diagram is about leadership. A capacity reading that will not load
  // drops the usage bar and nothing else.
  test("a failed storage read does not take the page down", async () => {
    mockedHa.getStorage.mockRejectedValue(new Error("admin api unreachable"));
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useHaStatus(), { wrapper });

    await waitFor(() => expect(result.current.status).toBeDefined());
    expect(result.current.error).toBe("");
    expect(storageUsage(result.current.usageSource)).toBeNull();
  });
});

describe("storageUsage", () => {
  test("prefers the MinIO cluster when there is one", () => {
    expect(
      storageUsage({
        backend: "s3",
        mode: "minio",
        blob_count: 0,
        blob_bytes: 0,
        dangling: [],
        minio: {
          total_capacity_bytes: 1000,
          used_bytes: 400,
          usage_ratio: 0.4,
        } as never,
      }),
    ).toEqual({ ratio: 0.4, usedBytes: 400, totalBytes: 1000 });
  });

  test("falls back to the volume for a filesystem backend", () => {
    expect(
      storageUsage({
        backend: "fs",
        mode: "filesystem",
        blob_count: 0,
        blob_bytes: 0,
        dangling: [],
        fs: { total_bytes: 200, used_bytes: 50, usage_ratio: 0.25, available_bytes: 150 },
      }),
    ).toEqual({ ratio: 0.25, usedBytes: 50, totalBytes: 200 });
  });

  // A bucket has no capacity to run out of, so a "94% full" reading there would
  // be inventing a limit that does not exist.
  test("plain S3 has no capacity to report", () => {
    expect(
      storageUsage({
        backend: "s3",
        mode: "s3",
        blob_count: 0,
        blob_bytes: 0,
        dangling: [],
      }),
    ).toBeNull();
  });

  test("a zero-capacity report is not a zero-percent bar", () => {
    expect(
      storageUsage({
        backend: "fs",
        mode: "filesystem",
        blob_count: 0,
        blob_bytes: 0,
        dangling: [],
        fs: { total_bytes: 0, used_bytes: 0, usage_ratio: 0, available_bytes: 0 },
      }),
    ).toBeNull();
  });
});

describe("formatUptime", () => {
  test("drops leading zero units but always shows seconds", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));

    expect(formatUptime("2026-06-01T11:59:15Z")).toBe("45s");
    expect(formatUptime("2026-06-01T11:52:30Z")).toBe("7m 30s");
    expect(formatUptime("2026-06-01T11:00:00Z")).toBe("1h 0m 0s");
    expect(formatUptime("2026-05-30T10:30:00Z")).toBe("2d 1h 30m 0s");
  });

  // Pod and browser clocks can disagree; "-3s of uptime" is worse than nothing.
  test("a start in the future reads as unknown", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));

    expect(formatUptime("2026-06-01T12:00:30Z")).toBe("-");
    expect(formatUptime("not a date")).toBe("-");
  });
});
