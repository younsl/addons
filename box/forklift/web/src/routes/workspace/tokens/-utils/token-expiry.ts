// Expiry is the one token attribute that decays: a date alone does not say
// whether it is a problem. These turn the timestamp into the two things the
// table needs - how long is left, and whether that is already none.

export const MAX_TTL_HOURS = 365 * 24;

export type TokenExpiry = {
  // "" when expired; the caller renders its own localised "expired" label,
  // because Intl would otherwise produce "0 minutes ago" for a dead token.
  label: string;
  isExpired: boolean;
};

// Localised to the app's language ("in 30 days" / "30일 후") so it matches the
// rest of the UI. numeric: "always" - "tomorrow" reads as friendlier than a
// credential's expiry deserves.
export function describeTokenExpiry(iso: string, language: string): TokenExpiry {
  const ms = new Date(iso).getTime() - Date.now();

  if (!isFinite(ms)) return { label: "", isExpired: false };
  if (ms <= 0) return { label: "", isExpired: true };

  const formatter = new Intl.RelativeTimeFormat(language, { numeric: "always" });
  const minutes = ms / 60_000;
  const hours = minutes / 60;
  const days = hours / 24;

  if (days >= 365) return { label: formatter.format(Math.round(days / 365), "year"), isExpired: false };
  if (days >= 60) return { label: formatter.format(Math.round(days / 30), "month"), isExpired: false };
  if (days >= 1) return { label: formatter.format(Math.round(days), "day"), isExpired: false };
  if (hours >= 1) return { label: formatter.format(Math.round(hours), "hour"), isExpired: false };

  // Never "in 0 minutes": a token with seconds left is still live, and rounding
  // it to zero would read as expired.
  return { label: formatter.format(Math.max(1, Math.round(minutes)), "minute"), isExpired: false };
}

// The API takes a duration, not a date, but a date is what a person picks. At
// least an hour (a token that expires on creation is useless) and at most the
// year the API enforces, so an out-of-range pick is corrected here rather than
// rejected by the server.
export function toExpiresIn(expiresOn: Date, startOfDay: (date: Date) => Date): string {
  const hours = Math.ceil((startOfDay(expiresOn).getTime() - Date.now()) / 3_600_000);

  return `${Math.min(Math.max(hours, 1), MAX_TTL_HOURS)}h`;
}

export function startOfLocalDay(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate());
}

// toISODate / parseISODate bridge the "YYYY-MM-DD" text input and the local
// Date the calendar works with.
export function toISODate(date: Date): string {
  const pad = (value: number) => String(value).padStart(2, "0");

  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

// Strict on purpose: "2026-02-31" parses in Date but is not a day, and a token
// silently expiring on March 3rd is worse than the field refusing the input.
// Returns local midnight, matching startOfLocalDay so range comparisons line up.
export function parseISODate(value: string): Date | undefined {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value.trim());
  if (!match) return undefined;

  const [year, month, day] = [Number(match[1]), Number(match[2]), Number(match[3])];
  const date = new Date(year, month - 1, day);

  if (date.getFullYear() !== year || date.getMonth() !== month - 1 || date.getDate() !== day) {
    return undefined;
  }

  return date;
}

// "YYYY-MM-DD HH:MM" straight off the RFC3339 string - date plus hours and
// minutes, which is the resolution "last used" is worth reading at.
export function stampMinute(iso: string): string {
  return iso.slice(0, 16).replace("T", " ");
}
