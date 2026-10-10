import { describe, expect, it } from "vitest";

import { formatMilliseconds } from "@/utils/format-duration";
import { formatFileSize } from "@/utils/format-file-size";
import {
  canEditRepositorySecurity,
  canReviewApprovals,
  canViewAccessManagement,
  canViewApprovalQueue,
} from "@/utils/permissions";
import { getRepositoryEndpoint } from "@/utils/repository-endpoint";

describe("formatFileSize", () => {
  it("stays in bytes below a kilobyte", () => {
    expect(formatFileSize(0)).toBe("0 B");
    expect(formatFileSize(1023)).toBe("1023 B");
  });

  it("climbs one unit at a time", () => {
    expect(formatFileSize(1024)).toBe("1.0 KB");
    expect(formatFileSize(1024 ** 3)).toBe("1.0 GB");
  });

  it("stops at the largest unit it knows", () => {
    expect(formatFileSize(1024 ** 5)).toBe("1024.0 TB");
  });
});

describe("formatMilliseconds", () => {
  it("switches unit at one second", () => {
    expect(formatMilliseconds(999)).toBe("999ms");
    expect(formatMilliseconds(1000)).toBe("1.0s");
  });
});

describe("getRepositoryEndpoint", () => {
  const origin = "https://forklift.example";

  it("gives each ecosystem the shape its tooling expects", () => {
    expect(getRepositoryEndpoint("cargo", "crates", origin).url).toBe(
      "sparse+https://forklift.example/cargo/crates/",
    );
    // go is the one without a trailing slash
    expect(getRepositoryEndpoint("go", "goproxy", origin).url).toBe(
      "https://forklift.example/go/goproxy",
    );
    expect(getRepositoryEndpoint("pypi", "pypi", origin).url).toBe(
      "https://forklift.example/pypi/pypi/simple/",
    );
  });

  it("falls back to a plain path for an unknown format", () => {
    const endpoint = getRepositoryEndpoint("raw", "files", origin);
    expect(endpoint.url).toBe("https://forklift.example/raw/files/");
    expect(endpoint.hint).toBe("");
  });
});

describe("permissions", () => {
  it("lets an auditor read the access surfaces but not the approval decision", () => {
    const auditor = { auditor: true };
    expect(canViewAccessManagement(auditor)).toBe(true);
    expect(canViewApprovalQueue(auditor)).toBe(true);
    expect(canReviewApprovals(auditor)).toBe(false);
  });

  it("separates reading the security policy from rewriting it", () => {
    const security = { security: true };
    expect(canEditRepositorySecurity(security)).toBe(true);
    expect(canViewAccessManagement(security)).toBe(false);
  });

  it("gives an admin everything", () => {
    const admin = { admin: true };
    expect(canViewAccessManagement(admin)).toBe(true);
    expect(canReviewApprovals(admin)).toBe(true);
    expect(canEditRepositorySecurity(admin)).toBe(true);
  });
});
