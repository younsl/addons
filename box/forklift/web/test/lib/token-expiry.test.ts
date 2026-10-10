import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import {
  MAX_TTL_HOURS,
  describeTokenExpiry,
  parseISODate,
  stampMinute,
  startOfLocalDay,
  toExpiresIn,
  toISODate,
} from "@/routes/workspace/tokens/-utils/token-expiry";

const NOW = new Date("2026-06-01T12:00:00Z");

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("describeTokenExpiry", () => {
  test("picks the largest unit that still reads sensibly", () => {
    expect(describeTokenExpiry("2028-06-01T12:00:00Z", "en").label).toBe("in 2 years");
    expect(describeTokenExpiry("2026-10-01T12:00:00Z", "en").label).toBe("in 4 months");
    expect(describeTokenExpiry("2026-06-08T12:00:00Z", "en").label).toBe("in 7 days");
    expect(describeTokenExpiry("2026-06-01T17:00:00Z", "en").label).toBe("in 5 hours");
    expect(describeTokenExpiry("2026-06-01T12:30:00Z", "en").label).toBe("in 30 minutes");
  });

  test("a token with seconds left is live, not expired", () => {
    // Rounding this to "in 0 minutes" would read as already gone.
    expect(describeTokenExpiry("2026-06-01T12:00:20Z", "en")).toEqual({
      label: "in 1 minute",
      isExpired: false,
    });
  });

  test("a past expiry is flagged and left unlabelled", () => {
    // The caller supplies its own localised "expired"; Intl would say
    // "1 day ago", which describes the date rather than the state.
    expect(describeTokenExpiry("2026-05-31T12:00:00Z", "en")).toEqual({
      label: "",
      isExpired: true,
    });
  });

  test("an unparseable timestamp is neither labelled nor called expired", () => {
    expect(describeTokenExpiry("not a date", "en")).toEqual({ label: "", isExpired: false });
  });

  test("follows the app language", () => {
    expect(describeTokenExpiry("2026-06-08T12:00:00Z", "ko").label).toContain("7");
  });
});

describe("toExpiresIn", () => {
  test("converts the picked day into the hour count the API takes", () => {
    const tomorrow = new Date(NOW);
    tomorrow.setDate(tomorrow.getDate() + 1);

    expect(toExpiresIn(tomorrow, startOfLocalDay)).toMatch(/^\d+h$/);
  });

  test("never issues a token that is already spent", () => {
    expect(toExpiresIn(new Date("2020-01-01T00:00:00Z"), startOfLocalDay)).toBe("1h");
  });

  test("caps at the year the API enforces", () => {
    const farFuture = new Date(NOW);
    farFuture.setFullYear(farFuture.getFullYear() + 10);

    expect(toExpiresIn(farFuture, startOfLocalDay)).toBe(`${MAX_TTL_HOURS}h`);
  });
});

describe("parseISODate", () => {
  test("reads a well-formed day as local midnight", () => {
    const parsed = parseISODate("2026-07-04");

    expect(parsed?.getFullYear()).toBe(2026);
    expect(parsed?.getMonth()).toBe(6);
    expect(parsed?.getDate()).toBe(4);
    expect(parsed?.getHours()).toBe(0);
  });

  // Date would roll this to March 3rd. A token silently expiring three days
  // later than asked is worse than the field refusing the input.
  test("rejects a day that does not exist", () => {
    expect(parseISODate("2026-02-31")).toBeUndefined();
  });

  test("rejects anything not exactly YYYY-MM-DD", () => {
    expect(parseISODate("2026-7-4")).toBeUndefined();
    expect(parseISODate("07/04/2026")).toBeUndefined();
    expect(parseISODate("")).toBeUndefined();
  });

  test("round-trips through toISODate", () => {
    expect(toISODate(parseISODate("2026-07-04")!)).toBe("2026-07-04");
  });
});

test("stampMinute keeps date, hours and minutes", () => {
  expect(stampMinute("2026-06-01T12:34:56Z")).toBe("2026-06-01 12:34");
});
