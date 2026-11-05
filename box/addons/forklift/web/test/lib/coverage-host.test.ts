import { describe, expect, it } from "vitest";
import { normalizeHost, validateExternalDomain } from "@/lib/coverage-host";

// These cases are the ones in src/coverage/host_test.rs. They are repeated
// here on purpose: the field validates locally for immediate feedback, and this
// is what keeps that copy honest against the server that actually enforces it.
describe("validateExternalDomain", () => {
  it("accepts external domains", () => {
    for (const host of [
      "forklift.example.org",
      "forklift.example.org:8443",
      "artifacts.corp.example.net",
      "10-0-0-5.example.org",
      "a.io",
    ]) {
      expect(validateExternalDomain(host), host).toBeNull();
    }
  });

  it("rejects what cannot be a forklift address", () => {
    for (const host of [
      "",
      "forklift.example.org/npm",
      "*.example.org",
      "user@forklift.example.org",
      "forklift.example.org:99999",
      "forklift.example.org:abc",
      "forklift example.org",
      "-leading-hyphen.example.org",
      "forklift.example.org,other.example.org",
      "forklift",
      "localhost",
      "localhost:8080",
      "forklift.svc",
      "forklift.forklift.svc.cluster.local",
      "db.internal",
      "127.0.0.1",
      "10.0.0.5",
      "169.254.169.254",
    ]) {
      expect(validateExternalDomain(host), host).not.toBeNull();
    }
  });

  it("names the reason so the field can explain itself", () => {
    expect(validateExternalDomain("")).toBe("required");
    expect(validateExternalDomain("forklift.example.org/npm")).toBe("not-bare");
    expect(validateExternalDomain("forklift.example.org:99999")).toBe("port-range");
    expect(validateExternalDomain("localhost")).toBe("not-external");
  });
});

describe("normalizeHost", () => {
  it("strips a scheme and trailing slashes", () => {
    expect(normalizeHost("https://forklift.example.com")).toBe("forklift.example.com");
    expect(normalizeHost("HTTP://forklift.example.com//")).toBe("forklift.example.com");
    expect(normalizeHost("  forklift.example.com  ")).toBe("forklift.example.com");
    expect(normalizeHost("forklift.example.com:8443")).toBe("forklift.example.com:8443");
  });
});
