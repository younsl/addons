import { describe, expect, test } from "vitest";

import { parseDateTime } from "@/components/app-ui/date-time-field";

describe("parseDateTime", () => {
  test("reads a typed local date and time", () => {
    expect(parseDateTime("2026-10-20", "18:30")?.getTime()).toBe(new Date(2026, 9, 20, 18, 30).getTime());
    expect(parseDateTime(" 2026-10-20 ", "9:05")?.getTime()).toBe(new Date(2026, 9, 20, 9, 5).getTime());
  });

  test("waits for a complete, real date and time", () => {
    expect(parseDateTime("2026-1", "18:30")).toBeNull();
    expect(parseDateTime("2026-02-30", "18:30")).toBeNull();
    expect(parseDateTime("2026-10-20", "24:00")).toBeNull();
    expect(parseDateTime("2026-10-20", "")).toBeNull();
  });
});
