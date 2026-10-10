// formatTimestamp renders a full local timestamp (year through second) with the
// viewer's timezone abbreviation, e.g. "2026-07-07 18:10:23 GMT+9".
//
// Used where the exact moment matters and a relative form would not do - "last
// updated" on a live status page, where "a few seconds ago" leaves it unclear
// whether the page is still refreshing.
export function formatTimestamp(date: Date): string {
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZoneName: "short",
  });
}
