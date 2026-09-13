import { describe, expect, it } from "vitest";
import { formatAbsolute, formatRelative } from "@/utils/format-relative";

// The instant is built relative to now, so the test does not depend on a clock
// the suite cannot control.
const ago = (ms: number) => new Date(Date.now() - ms).toISOString();
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

describe("formatRelative", () => {
  it("steps up through the units", () => {
    expect(formatRelative(ago(0))).toBe("just now");
    expect(formatRelative(ago(5 * MIN))).toBe("5m ago");
    expect(formatRelative(ago(3 * HOUR))).toBe("3h ago");
    expect(formatRelative(ago(4 * DAY))).toBe("4d ago");
    // Past a month the label counts months, in 30-day steps so it stays
    // monotonic.
    expect(formatRelative(ago(45 * DAY))).toBe("1mo ago");
    expect(formatRelative(ago(400 * DAY))).toBe("13mo ago");
  });

  it("has an answer for a missing or unusable instant", () => {
    expect(formatRelative(null)).toBe("never");
    expect(formatRelative(undefined)).toBe("never");
    expect(formatRelative("")).toBe("never");
    expect(formatRelative("not a date")).toBe("never");
  });
});

// The detail page shows the exact instant, which has to carry its zone: the same
// timestamp reads as two different clock times to two readers, and one without a
// zone silently picks one of them.
describe("formatAbsolute", () => {
  const iso = "2026-08-01T00:00:00Z";

  it("includes a timezone name", () => {
    // The runner's zone decides the wording, so assert the shape rather than a
    // fixed string: a long time style always names the zone.
    const out = formatAbsolute(iso);
    expect(out).not.toBe("");
    expect(out).toMatch(/2026/);
    expect(out.split(" ").length).toBeGreaterThan(3);
  });

  it("follows the selected language, not the browser locale", () => {
    expect(formatAbsolute(iso, "ko")).not.toBe(formatAbsolute(iso, "en"));
  });

  it("has an answer for a missing or unusable instant", () => {
    expect(formatAbsolute(null)).toBe("");
    expect(formatAbsolute(undefined)).toBe("");
    expect(formatAbsolute("")).toBe("");
    expect(formatAbsolute("not a date")).toBe("");
  });
});
