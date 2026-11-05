import { describe, expect, test } from "vitest";

import { ACTIONS } from "@/lib/role-actions";
import {
  DEFAULT_ROLE_ACTIONS,
  appendRolePermission,
  canAddRolePermission,
  formatRolePermission,
  removeRolePermissionAt,
  toggleRoleAction,
} from "@/routes/access/roles/-utils/role-permissions";

describe("role permission editing", () => {
  test("toggling an action adds it, toggling again removes it", () => {
    expect(toggleRoleAction(["read"], "write")).toEqual(["read", "write"]);
    expect(toggleRoleAction(["read", "write"], "write")).toEqual(["read"]);
  });

  test("toggling does not mutate the array it was given", () => {
    const actions = ["read" as const];

    toggleRoleAction(actions, "write");

    expect(actions).toEqual(["read"]);
  });

  test("a permission needs both a pattern and at least one action", () => {
    expect(canAddRolePermission({ actions: ["read"], pattern: "maven-*" })).toBe(true);
    // Whitespace is not a pattern; the API would store a grant matching nothing.
    expect(canAddRolePermission({ actions: ["read"], pattern: "   " })).toBe(false);
    expect(canAddRolePermission({ actions: [], pattern: "maven-*" })).toBe(false);
  });

  test("appending trims the pattern and copies the actions", () => {
    const actions = ["read" as const];
    const [permission] = appendRolePermission({
      actions,
      pattern: "  maven-releases  ",
      permissions: [],
    });

    expect(permission).toEqual({ repo_pattern: "maven-releases", actions: ["read"] });

    // The row's action state is reused for the next entry, so the stored
    // permission must not alias it - editing the row would rewrite history.
    actions.push("write" as never);
    expect(permission.actions).toEqual(["read"]);
  });

  test("appending an invalid entry leaves the list untouched", () => {
    const permissions = [{ repo_pattern: "a", actions: ["read" as const] }];

    expect(appendRolePermission({ actions: [], pattern: "", permissions })).toBe(permissions);
  });

  test("removing by index keeps duplicate patterns apart", () => {
    const permissions = [
      { repo_pattern: "maven-*", actions: ["read" as const] },
      { repo_pattern: "maven-*", actions: ["write" as const] },
    ];

    expect(removeRolePermissionAt(permissions, 0)).toEqual([
      { repo_pattern: "maven-*", actions: ["write"] },
    ]);
  });

  test("a permission reads as pattern then actions", () => {
    expect(formatRolePermission({ repo_pattern: "maven-*", actions: ["read", "write"] }))
      .toBe("maven-*: read,write");
  });

  test("read is the default grant", () => {
    expect(DEFAULT_ROLE_ACTIONS).toEqual(["read"]);
    expect(ACTIONS).toContain("read");
  });
});
