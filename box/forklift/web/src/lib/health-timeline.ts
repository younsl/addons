import type { StorageHealthCheck } from "@/services/v1/openapi-types";

// healthSlots buckets checks into `count` fixed-width slots ending at `now`,
// oldest first, so bars keep their place on the time axis even when a check is
// missing. A slot without a check is null. When two checks land in one slot
// the newer one wins.
export function healthSlots(
  checks: StorageHealthCheck[],
  now: number,
  intervalMs: number,
  count: number,
): (StorageHealthCheck | null)[] {
  const slots: (StorageHealthCheck | null)[] = Array(count).fill(null);
  for (const check of checks) {
    const age = now - Date.parse(check.at);
    if (Number.isNaN(age) || age < 0) continue;
    const back = Math.floor(age / intervalMs);
    if (back >= count) continue;
    slots[count - 1 - back] = check;
  }
  return slots;
}

// axisTicks returns `n` evenly spaced instants from the start of the window to
// `now`, oldest first.
export function axisTicks(now: number, intervalMs: number, count: number, n: number): Date[] {
  const span = intervalMs * count;
  return Array.from({ length: n }, (_, i) => new Date(now - span + (span * i) / (n - 1)));
}

export function uptimeRatio(checks: StorageHealthCheck[]): number | null {
  if (checks.length === 0) return null;
  return checks.filter((c) => c.ok).length / checks.length;
}

export function averageLatency(checks: StorageHealthCheck[]): number | null {
  const ok = checks.filter((c) => c.ok);
  if (ok.length === 0) return null;
  return Math.round(ok.reduce((sum, c) => sum + c.latency_ms, 0) / ok.length);
}
