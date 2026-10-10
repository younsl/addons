import { describe, expect, it } from "vitest";
import {
  isMuted,
  readExclusionReason,
  toggleMuteScope,
} from "@/lib/coverage-exclusion";

describe("readExclusionReason", () => {
  it("separates the console from the repository's own topic", () => {
    expect(readExclusionReason("muted")?.key).toBe("coverage.muted-by-console");
    expect(readExclusionReason("topic:forklift.excluded")).toEqual({
      key: "coverage.muted-by-topic",
      topic: "forklift.excluded",
    });
    expect(readExclusionReason("")).toBeNull();
  });
});

describe("toggleMuteScope", () => {
  it("returns the whole selection rather than a delta", () => {
    expect(toggleMuteScope([], "ci", true)).toEqual(["ci"]);
    expect(toggleMuteScope(["registry"], "ci", true)).toEqual(["ci", "registry"]);
    expect(toggleMuteScope(["ci", "registry"], "ci", false)).toEqual(["registry"]);
    expect(toggleMuteScope(["ci"], "ci", false)).toEqual([]);
  });

  it("keeps the order stable and drops anything it does not know", () => {
    expect(toggleMuteScope(["registry", "nope"], "ci", true)).toEqual(["ci", "registry"]);
  });
});

describe("isMuted", () => {
  it("reads one check out of the list", () => {
    expect(isMuted(["registry"], "registry")).toBe(true);
    expect(isMuted(["registry"], "ci")).toBe(false);
    expect(isMuted(undefined, "ci")).toBe(false);
  });
});
