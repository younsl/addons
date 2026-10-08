import { describe, expect, it } from "vitest";

import { averageLatency, axisTicks, healthSlots, uptimeRatio } from "@/lib/health-timeline";

const MIN = 60_000;
const now = Date.parse("2026-10-08T12:00:00Z");
const at = (minutesAgo: number, ok = true, latency_ms = 10) => ({
  at: new Date(now - minutesAgo * MIN).toISOString(),
  ok,
  latency_ms,
});

describe("healthSlots", () => {
  it("places each check by its age, newest at the right edge", () => {
    const slots = healthSlots([at(2.5), at(0.2, false)], now, MIN, 5);
    expect(slots.map((s) => s && s.ok)).toEqual([null, null, true, null, false]);
  });

  it("drops checks outside the window or from the future", () => {
    const slots = healthSlots([at(5), at(10), at(-1)], now, MIN, 5);
    expect(slots.every((s) => s === null)).toBe(true);
  });

  it("keeps the newer check when two share a slot", () => {
    const slots = healthSlots([at(0.9, false), at(0.1, true)], now, MIN, 3);
    expect(slots[2]?.ok).toBe(true);
  });

  it("ignores unparseable timestamps", () => {
    const slots = healthSlots([{ at: "nope", ok: true, latency_ms: 1 }], now, MIN, 3);
    expect(slots).toEqual([null, null, null]);
  });
});

describe("axisTicks", () => {
  it("spans the window evenly and ends at now", () => {
    const ticks = axisTicks(now, MIN, 60, 5);
    expect(ticks.map((d) => (now - d.getTime()) / MIN)).toEqual([60, 45, 30, 15, 0]);
  });
});

describe("uptimeRatio and averageLatency", () => {
  it("are null without checks", () => {
    expect(uptimeRatio([])).toBeNull();
    expect(averageLatency([])).toBeNull();
  });

  it("average only successful checks", () => {
    const checks = [at(3, true, 10), at(2, true, 21), at(1, false, 5000), at(0, true, 30)];
    expect(uptimeRatio(checks)).toBe(0.75);
    expect(averageLatency(checks)).toBe(20);
    expect(averageLatency([at(0, false)])).toBeNull();
  });
});
