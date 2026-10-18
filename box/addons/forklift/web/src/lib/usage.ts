// Utilization thresholds for every "how full is the store" bar: the HA topology
// diagram and the Storage page draw the same volume, so they share the numbers
// here rather than each carrying their own copy. One volume must never look
// healthy on one screen and critical on the other.
export const USAGE_WARNING_PCT = 70;
export const USAGE_CRITICAL_PCT = 90;

export type UsageTone = "ok" | "warning" | "critical";

export function usageTone(pct: number): UsageTone {
  if (pct >= USAGE_CRITICAL_PCT) return "critical";
  if (pct >= USAGE_WARNING_PCT) return "warning";
  return "ok";
}

// One part of a usage bar: a repository format, forklift's remaining blobs,
// objects other clients keep in the store, or disk space outside object data.
export type UsageSegment = { key: string; format?: string; bytes: number; blobCount?: number; color: string; hatched?: boolean };

const SERIES = ["var(--fx-series-1)", "var(--fx-series-2)", "var(--fx-series-3)", "var(--fx-series-4)"];

// usageSegments splits a store's used bytes into the parts the bar draws:
// the largest formats each in their own colour, the rest of forklift's blobs
// (smaller formats and blobs awaiting cleanup) as one part, then what is not
// forklift's. With the store's logical object bytes that splits in two: other
// data is objects other clients stored, system data is disk the objects do not
// account for (replicas or parity, unreclaimed deletes, files beside the
// store). Without them the two cannot be told apart and stay one part. Parts
// are largest-format first and never negative.
export function usageSegments(
  formats: { format: string; bytes: number; blob_count: number }[],
  blobBytes: number,
  usedBytes: number,
  logicalBytes?: number,
): UsageSegment[] {
  const top = formats.slice(0, SERIES.length).filter((f) => f.bytes > 0);
  const out: UsageSegment[] = top.map((f, i) => ({
    key: `format-${f.format}`,
    format: f.format,
    bytes: f.bytes,
    blobCount: f.blob_count,
    color: SERIES[i],
  }));
  const shown = top.reduce((n, f) => n + f.bytes, 0);
  const rest = Math.max(0, blobBytes - shown);
  if (rest > 0) out.push({ key: "forklift-other", bytes: rest, color: "var(--fx-series-other)" });
  const forklift = Math.max(blobBytes, shown);
  if (logicalBytes === undefined) {
    const rest = Math.max(0, usedBytes - forklift);
    if (rest > 0) out.push({ key: "cluster-rest", bytes: rest, color: "var(--fx-border-strong)" });
    return out;
  }
  const other = Math.max(0, logicalBytes - forklift);
  if (other > 0) out.push({ key: "cluster-other", bytes: other, color: "var(--fx-border-strong)" });
  const system = Math.max(0, usedBytes - Math.max(logicalBytes, forklift));
  if (system > 0) out.push({ key: "system", bytes: system, color: "var(--fx-border-strong)", hatched: true });
  return out;
}

// segmentWidths gives each part its share of the track in percent. forklift
// counts logical bytes and a cluster may report less raw usage, so when the
// parts outgrow the used share they are scaled down to fit it.
export function segmentWidths(segments: UsageSegment[], totalBytes: number, usagePct: number): number[] {
  if (totalBytes <= 0) return segments.map(() => 0);
  const raw = segments.map((s) => (s.bytes / totalBytes) * 100);
  const sum = raw.reduce((n, w) => n + w, 0);
  const cap = Math.min(100, Math.max(0, usagePct));
  return sum > cap && sum > 0 ? raw.map((w) => (w * cap) / sum) : raw;
}
