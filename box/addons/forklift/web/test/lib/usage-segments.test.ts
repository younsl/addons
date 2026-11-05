import { describe, expect, test } from "vitest";

import { segmentWidths, usageSegments } from "@/lib/usage";

const f = (format: string, bytes: number) => ({ format, bytes, blob_count: 1 });

describe("usageSegments", () => {
  test("gives the four largest formats their own colour and folds the rest", () => {
    const parts = usageSegments(
      [f("oci", 500), f("maven", 300), f("npm", 100), f("pypi", 50), f("raw", 20), f("go", 10)],
      1000,
      1500,
    );
    expect(parts.map((p) => [p.key, p.bytes, p.color])).toEqual([
      ["format-oci", 500, "var(--fx-series-1)"],
      ["format-maven", 300, "var(--fx-series-2)"],
      ["format-npm", 100, "var(--fx-series-3)"],
      ["format-pypi", 50, "var(--fx-series-4)"],
      // raw + go + 20 bytes no artifact references
      ["forklift-other", 50, "var(--fx-series-other)"],
      ["cluster-rest", 500, "var(--fx-border-strong)"],
    ]);
  });

  test("splits what is not forklift's into other data and system data", () => {
    const parts = usageSegments([f("oci", 300)], 300, 2000, 800);
    expect(parts.map((p) => [p.key, p.bytes, Boolean(p.hatched)])).toEqual([
      ["format-oci", 300, false],
      // objects other clients stored: 800 logical - 300 forklift
      ["cluster-other", 500, false],
      // disk the objects do not account for: 2000 used - 800 logical
      ["system", 1200, true],
    ]);
  });

  test("a store holding only forklift's objects has no other data", () => {
    const parts = usageSegments([f("oci", 300)], 300, 1000, 280);
    expect(parts.map((p) => [p.key, p.bytes])).toEqual([
      ["format-oci", 300],
      ["system", 700],
    ]);
  });

  test("never reports a negative remainder", () => {
    const parts = usageSegments([f("raw", 100)], 100, 60);
    expect(parts.map((p) => p.key)).toEqual(["format-raw"]);
  });

  test("an empty store has no parts", () => {
    expect(usageSegments([], 0, 0)).toEqual([]);
  });
});

describe("segmentWidths", () => {
  test("is each part's share of capacity", () => {
    const parts = usageSegments([f("raw", 25)], 25, 50);
    expect(segmentWidths(parts, 100, 50)).toEqual([25, 25]);
  });

  test("scales parts down to the used share when they outgrow it", () => {
    const parts = usageSegments([f("raw", 80)], 80, 40);
    expect(segmentWidths(parts, 100, 40)).toEqual([40]);
  });
});
