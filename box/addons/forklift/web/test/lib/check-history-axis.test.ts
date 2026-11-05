import { describe, expect, test } from "vitest";

import { axisTicks, runLabels, stretches, yTicks } from "@/routes/workspace/insights/-components/check-history";
import type { InsightCheckPoint } from "@/services/v1/openapi-types";

const local = (y: number, mo: number, d: number, h: number, mi: number, sec = 0) => new Date(y, mo - 1, d, h, mi, sec).toISOString();

describe("runLabels", () => {
  test("uses minutes while they tell the runs apart", () => {
    expect(runLabels([local(2026, 10, 17, 9, 5), local(2026, 10, 17, 14, 46)])).toEqual(["10-17 09:05", "14:46"]);
  });

  test("adds seconds when two runs share a minute", () => {
    expect(runLabels([local(2026, 10, 17, 23, 46, 1), local(2026, 10, 17, 23, 46, 9), local(2026, 10, 17, 23, 51, 0)])).toEqual(["10-17 23:46:01", "23:46:09", "23:51:00"]);
  });

  test("dates the first run of each new day", () => {
    expect(runLabels([local(2026, 10, 17, 22, 0), local(2026, 10, 17, 23, 0), local(2026, 10, 18, 0, 0)])).toEqual(["10-17 22:00", "23:00", "10-18 00:00"]);
  });

  test("tells apart runs in the same minute of different days", () => {
    expect(runLabels([local(2026, 10, 17, 9, 0), local(2026, 10, 18, 9, 0)])).toEqual(["10-17 09:00", "10-18 09:00"]);
  });
});

describe("stretches", () => {
  test("folds consecutive runs with the same status", () => {
    const at = (s: number) => local(2026, 10, 17, 23, 46, s);
    const points = [
      { status: "acknowledged", generated_at: at(1) },
      { status: "action", generated_at: at(5) },
      { status: "action", generated_at: at(9) },
      { status: "acknowledged", generated_at: at(18) },
    ].map((p) => ({ ...p, check_id: "x", severity: "high", flagged: 1 })) as InsightCheckPoint[];
    expect(stretches(points, ["a", "b", "c", "d"])).toEqual([
      { status: "acknowledged", from: at(1), label: "a", runs: 1 },
      { status: "action", from: at(5), label: "b", runs: 2 },
      { status: "acknowledged", from: at(18), label: "d", runs: 1 },
    ]);
  });
});

describe("yTicks", () => {
  test("adds a midpoint once there is a whole number between top and floor", () => {
    expect(yTicks(1)).toEqual([1, 0]);
    expect(yTicks(2)).toEqual([2, 1, 0]);
    expect(yTicks(9)).toEqual([9, 5, 0]);
  });
});

describe("axisTicks", () => {
  const hourly = (n: number) => Array.from({ length: n }, (_, i) => new Date(Date.UTC(2026, 8, 18, 0, 0) + i * 3_600_000).toISOString());

  test("labels never overlap, even over hundreds of runs", () => {
    for (const n of [7, 48, 720]) {
      for (const width of [240, 480, 900]) {
        const ticks = axisTicks(hourly(n), width);
        expect(ticks.length).toBeGreaterThan(0);
        ticks.forEach((tick, i) => {
          const w = Math.max(...tick.lines.map((l) => l.length)) * 6.5;
          expect(tick.left).toBeGreaterThanOrEqual(0);
          expect(tick.left + w).toBeLessThanOrEqual(width + 0.001);
          if (i > 0) {
            const prev = ticks[i - 1];
            const prevW = Math.max(...prev.lines.map((l) => l.length)) * 6.5;
            expect(prev.left + prevW + 8).toBeLessThanOrEqual(tick.left + 0.001);
          }
        });
      }
    }
  });

  test("names a window of days by day, once per day", () => {
    const ticks = axisTicks(hourly(720), 900);
    expect(ticks.every((tick) => /^\d{2}-\d{2}$/.test(tick.lines[0]))).toBe(true);
    expect(new Set(ticks.map((tick) => tick.lines[0])).size).toBe(ticks.length);
  });
});

describe("stretches over thousands of runs", () => {
  test("buckets runs and keeps the most urgent status of each bucket", () => {
    const at = (i: number) => new Date(Date.UTC(2026, 8, 18) + i * 300_000).toISOString();
    const points = Array.from({ length: 8640 }, (_, i) => ({
      check_id: "x", severity: "low", flagged: 1, generated_at: at(i),
      status: i === 4321 ? "action" : "ok",
    })) as InsightCheckPoint[];
    const bands = stretches(points, points.map((p) => p.generated_at), 300);
    expect(bands.length).toBeLessThanOrEqual(300);
    expect(bands.reduce((n, b) => n + b.runs, 0)).toBe(8640);
    expect(bands.some((b) => b.status === "action")).toBe(true);
  });
});
