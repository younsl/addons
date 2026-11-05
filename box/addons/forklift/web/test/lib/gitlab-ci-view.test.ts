import { describe, expect, it } from "vitest";
import { lineReferencesForklift } from "@/components/app-ui/gitlab-ci-view";

// The viewer marks the lines the verdict was based on, so this rule has to match
// bodyMatches in src/coverage/scan.rs: the host, or the credential a job
// authenticates with when the host is never named inline.
describe("lineReferencesForklift", () => {
  const host = "forklift.example.com";

  it("marks a line naming the host", () => {
    expect(lineReferencesForklift("  image: forklift.example.com/docker/base:1", host)).toBe(true);
    expect(lineReferencesForklift("registry=https://forklift.example.com/npm/npmjs/", host)).toBe(true);
  });

  it("marks a line using the forklift credential", () => {
    expect(lineReferencesForklift("    - echo $FORKLIFT_DEPLOY_TOKEN", host)).toBe(true);
    // The credential counts even with no host configured, which is how a scan
    // with only a token reference still reads as wired.
    expect(lineReferencesForklift("  password: ${FORKLIFT_TOKEN}", "")).toBe(true);
  });

  it("leaves ordinary lines alone", () => {
    expect(lineReferencesForklift("  image: node:24-alpine", host)).toBe(false);
    expect(lineReferencesForklift("stages:", host)).toBe(false);
    // A similar-looking variable is not the forklift credential.
    expect(lineReferencesForklift("  echo $FORKLIFT_HOME", host)).toBe(false);
  });

  it("does not match the host when none is configured", () => {
    expect(lineReferencesForklift("  image: forklift.example.com/x", "")).toBe(false);
  });
});
