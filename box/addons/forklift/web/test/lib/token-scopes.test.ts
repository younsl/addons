import { describe, expect, test } from "vitest";

import {
  formatTokenScope,
  formatTokenScopes,
  parseTokenScopes,
} from "@/utils/token-scopes";

// scopes_json is a JSON string on the wire, so anything can be in it - a token
// written before the feature existed, or by an older server. A malformed value
// means "no scopes recorded", never a broken page.
describe("parseTokenScopes", () => {
  test("reads a scope list", () => {
    expect(parseTokenScopes('[{"repo_pattern":"maven-*","actions":["read"]}]')).toEqual([
      { repo_pattern: "maven-*", actions: ["read"] },
    ]);
  });

  test("an empty, malformed or non-array value reads as no scopes", () => {
    expect(parseTokenScopes("")).toEqual([]);
    expect(parseTokenScopes("not json")).toEqual([]);
    expect(parseTokenScopes("null")).toEqual([]);
    expect(parseTokenScopes('{"repo_pattern":"maven-*"}')).toEqual([]);
  });
});

describe("formatting", () => {
  test("a scope reads as pattern then actions", () => {
    expect(formatTokenScope({ repo_pattern: "maven-*", actions: ["read", "write"] }))
      .toBe("maven-*: read,write");
  });

  test("a whole list joins for sorting", () => {
    expect(
      formatTokenScopes('[{"repo_pattern":"a","actions":["read"]},{"repo_pattern":"b","actions":["write"]}]'),
    ).toBe("a: read, b: write");
  });
});
