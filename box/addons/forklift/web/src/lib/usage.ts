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
