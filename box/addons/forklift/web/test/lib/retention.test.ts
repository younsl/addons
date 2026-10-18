import { describe, expect, test } from "vitest";

import { idleTtlDays, idleTtlFromDays } from "@/lib/retention";

describe("idleTtlDays", () => {
  test("reads typed and server-formatted durations as days", () => {
    expect(idleTtlDays("30d")).toBe(30);
    expect(idleTtlDays("2w")).toBe(14);
    expect(idleTtlDays("720h0m0s")).toBe(30);
    expect(idleTtlDays("36h")).toBe(1.5);
  });

  test("treats empty and zero as retention off", () => {
    expect(idleTtlDays(undefined)).toBe(0);
    expect(idleTtlDays("")).toBe(0);
    expect(idleTtlDays("0")).toBe(0);
  });

  test("rejects text that is not a duration", () => {
    expect(idleTtlDays("soon")).toBeNaN();
    expect(idleTtlDays("30dx")).toBeNaN();
  });
});

describe("idleTtlFromDays", () => {
  test("writes whole days and turns zero into off", () => {
    expect(idleTtlFromDays(7)).toBe("7d");
    expect(idleTtlFromDays(0)).toBe("0");
  });
});
