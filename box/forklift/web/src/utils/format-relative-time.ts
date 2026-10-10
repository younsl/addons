// formatRelativeTime renders a localized distance from now to an ISO timestamp
// ("in 11 months", "3 days ago"), shown alongside an absolute date rather than
// instead of it: the relative form answers "is this soon?" at a glance, the
// absolute one answers "when exactly?".
//
// The unit is the largest that still reads sensibly, because "in 340 days" is
// harder to judge than "in 11 months".
export function formatRelativeTime(iso: string, language: string): string {
  const ms = new Date(iso).getTime() - Date.now();
  const formatter = new Intl.RelativeTimeFormat(language, { numeric: "auto" });
  const days = Math.round(ms / 86_400_000);

  if (Math.abs(ms) < 86_400_000) return formatter.format(Math.round(ms / 3_600_000), "hour");
  if (Math.abs(days) < 45) return formatter.format(days, "day");
  // 545 days is 18 months: past that, counting months stops being informative.
  if (Math.abs(days) < 545) return formatter.format(Math.round(days / 30), "month");

  return formatter.format(Math.round(days / 365), "year");
}
