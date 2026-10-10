// formatRelative renders how long ago an instant was, in the short form a table
// column can hold. Ported from the Backstage plugin so one instant never reads
// as two different ages depending on which page is open.
export function formatRelative(iso: string | null | undefined): string {
  if (!iso) return "never";
  const minutes = Math.floor((Date.now() - new Date(iso).getTime()) / 60_000);
  if (!Number.isFinite(minutes)) return "never";
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  // Past a month the day count stops being a fact anybody reads and turns into a
  // number to divide, so months are counted instead. 30-day months keep the
  // label monotonic, which a calendar month would not.
  return `${Math.floor(days / 30)}mo ago`;
}

// formatAbsolute renders the full instant, including the timezone it is being
// read in. The relative form above answers "is this stale"; this one answers
// "when exactly", which needs the zone to be unambiguous: the same instant reads
// as two different clock times to two readers, and a timestamp without a zone
// silently picks one of them.
export function formatAbsolute(
  iso: string | null | undefined,
  language: "en" | "ko" = "en"
): string {
  if (!iso) return "";
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "";
  return at.toLocaleString(language === "ko" ? "ko-KR" : "en-US", {
    dateStyle: "medium",
    timeStyle: "long",
  });
}
