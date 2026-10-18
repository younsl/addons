// The server stores retention.idle_ttl as a duration string: "30d" as typed,
// or Go-style ("720h0m0s") once it has been saved. The console edits it in
// whole days, so these convert between the two.

const DAY_MS = 86_400_000;

const UNIT_MS: Record<string, number> = {
  ns: 1e-6,
  us: 1e-3,
  "µs": 1e-3,
  ms: 1,
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
  d: DAY_MS,
  w: 7 * DAY_MS,
};

// Returns the duration in days (fractional when it is not a whole number of
// days), 0 when retention is off, and NaN when the value is not a duration.
export function idleTtlDays(value?: string | null): number {
  const text = (value ?? "").trim();
  if (text === "" || text === "0") return 0;

  const pattern = /(\d+(?:\.\d+)?)(ns|us|µs|ms|s|m|h|d|w)/gy;
  let ms = 0;
  let end = 0;
  for (let match = pattern.exec(text); match; match = pattern.exec(text)) {
    ms += Number(match[1]) * UNIT_MS[match[2]];
    end = pattern.lastIndex;
  }
  return end === text.length ? ms / DAY_MS : Number.NaN;
}

export function idleTtlFromDays(days: number): string {
  return days > 0 ? `${days}d` : "0";
}
