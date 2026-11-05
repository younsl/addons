import { describe, expect, it } from "vitest";
import { formatMilliseconds } from "@/utils/format-duration";

// The DNS and connection checks both report their elapsed time through this, so
// a slow lookup reads as seconds rather than a four-digit millisecond count.
describe("formatMilliseconds", () => {
  it("uses milliseconds below one second", () => {
    expect(formatMilliseconds(0)).toBe("0ms");
    expect(formatMilliseconds(73)).toBe("73ms");
    expect(formatMilliseconds(999)).toBe("999ms");
  });

  it("switches to seconds from one second up", () => {
    expect(formatMilliseconds(1000)).toBe("1.0s");
    expect(formatMilliseconds(1240)).toBe("1.2s");
    expect(formatMilliseconds(5000)).toBe("5.0s");
  });
});
