import type { ArtifactBulkResult } from "@/services/v1/openapi-types";

// The server's per-request cap. A selection larger than this is sent as several
// requests, which is also what keeps one click from holding the single database
// writer for minutes.
export const ARTIFACT_BULK_CHUNK = 200;

// ArtifactBulkOutcome is a whole selection's result, however many requests it
// took: what changed, and every path that refused.
export type ArtifactBulkOutcome = {
  requested: number;
  succeeded: number;
  failed: ArtifactBulkResult["failed"];
};

// runArtifactBulk sends a selection in batches and folds the answers into one.
//
// The batches run one after another rather than at once: they contend for the
// same writer, so overlapping them would not finish sooner, and a serial run
// leaves the failure list in the order the reader selected.
export async function runArtifactBulk(
  paths: string[],
  send: (batch: string[]) => Promise<ArtifactBulkResult>,
): Promise<ArtifactBulkOutcome> {
  const out: ArtifactBulkOutcome = { requested: paths.length, succeeded: 0, failed: [] };
  for (let i = 0; i < paths.length; i += ARTIFACT_BULK_CHUNK) {
    const result = await send(paths.slice(i, i + ARTIFACT_BULK_CHUNK));
    out.succeeded += result.succeeded;
    out.failed = out.failed.concat(result.failed);
  }
  return out;
}

// bulkRetryPaths reports what refused, in the order it was selected. It is what
// the selection becomes after a batch: whatever went through is done with, and
// leaving the rest selected makes the obvious next click a retry of exactly
// those.
export function bulkRetryPaths(paths: string[], outcome: ArtifactBulkOutcome): string[] {
  const failed = new Set(outcome.failed.map((f) => f.path));
  return paths.filter((path) => failed.has(path));
}
