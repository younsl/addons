import { describe, expect, it, vi } from "vitest";
import type { ArtifactBulkResult } from "@/services/v1/openapi-types";
import {
  ARTIFACT_BULK_CHUNK,
  bulkRetryPaths,
  runArtifactBulk,
} from "@/lib/artifact-bulk";

const ok = (paths: string[]): ArtifactBulkResult => ({
  requested: paths.length,
  succeeded: paths.length,
  failed: [],
});

describe("runArtifactBulk", () => {
  it("sends one request when the selection fits the cap", async () => {
    const send = vi.fn(async (batch: string[]) => ok(batch));
    const outcome = await runArtifactBulk(["a", "b"], send);

    expect(send).toHaveBeenCalledTimes(1);
    expect(outcome).toEqual({ requested: 2, succeeded: 2, failed: [] });
  });

  it("splits a selection larger than the cap and folds the answers", async () => {
    const paths = Array.from({ length: ARTIFACT_BULK_CHUNK + 5 }, (_, i) => `p-${i}`);
    const batches: number[] = [];
    const outcome = await runArtifactBulk(paths, async (batch) => {
      batches.push(batch.length);
      return ok(batch);
    });

    expect(batches).toEqual([ARTIFACT_BULK_CHUNK, 5]);
    expect(outcome.requested).toBe(paths.length);
    expect(outcome.succeeded).toBe(paths.length);
  });

  it("keeps every failure, across batches", async () => {
    const paths = Array.from({ length: ARTIFACT_BULK_CHUNK + 1 }, (_, i) => `p-${i}`);
    const outcome = await runArtifactBulk(paths, async (batch) => ({
      requested: batch.length,
      succeeded: batch.length - 1,
      failed: [{ path: batch[0], error: "not found" }],
    }));

    expect(outcome.succeeded).toBe(paths.length - 2);
    expect(outcome.failed).toHaveLength(2);
  });
});

describe("bulkRetryPaths", () => {
  it("keeps the refused paths, in the order they were selected", () => {
    const outcome = {
      requested: 3,
      succeeded: 1,
      failed: [{ path: "c", error: "not found" }, { path: "a", error: "forbidden" }],
    };
    expect(bulkRetryPaths(["a", "b", "c"], outcome)).toEqual(["a", "c"]);
  });
});
