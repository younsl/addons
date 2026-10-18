import { describe, expect, test } from "vitest";

import { fractionOf, indexAt, zoneLabel } from "@/routes/workspace/insights/-components/chart-hover";

describe("chart hover", () => {
  test("snaps to the slot under the pointer", () => {
    expect(indexAt(0, 4, "slot")).toBe(0);
    expect(indexAt(0.49, 4, "slot")).toBe(1);
    expect(indexAt(1, 4, "slot")).toBe(3);
  });

  test("snaps to the nearest run on an edge layout", () => {
    expect(indexAt(0.2, 3, "edge")).toBe(0);
    expect(indexAt(0.3, 3, "edge")).toBe(1);
    expect(indexAt(0.9, 3, "edge")).toBe(2);
  });

  test("places runs at slot centres or on the edges", () => {
    expect(fractionOf(0, 4, "slot")).toBe(0.125);
    expect(fractionOf(2, 3, "edge")).toBe(1);
    expect(fractionOf(0, 1, "edge")).toBe(0.5);
  });
});

describe("zoneLabel", () => {
  const at = new Date("2026-10-17T14:46:01Z");

  test("names Seoul time KST", () => {
    expect(zoneLabel(at, "Asia/Seoul")).toBe("KST");
  });

  test("names UTC as UTC", () => {
    expect(zoneLabel(at, "UTC")).toBe("UTC");
  });

  test("falls back to the offset from UTC", () => {
    expect(zoneLabel(at, "Asia/Kolkata")).toBe("UTC+5:30");
  });
});
