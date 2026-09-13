import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { ApiError } from "@/lib/http/error/api-error";
import * as rolesApi from "@/services/v1/roles/api";
import * as usersApi from "@/services/v1/users/api";
import { useRoleDetail } from "@/routes/access/roles/-hooks/use-role-detail";
import {
  useAddRolePermissionMutation,
  useRemoveRolePermissionMutation,
} from "@/routes/access/roles/-hooks/use-role-mutations";

vi.mock("@/services/v1/roles/api");
vi.mock("@/services/v1/users/api");

const mockedRoles = vi.mocked(rolesApi);
const mockedUsers = vi.mocked(usersApi);

const role = {
  id: 7,
  name: "maven-readers",
  description: "",
  created_at: "2026-01-01T00:00:00Z",
  managed: false,
  permissions: [{ id: 3, repo_pattern: "maven-*", actions: ["read"] }],
  user_count: 1,
};

const member = {
  id: 1,
  username: "alice",
  source: "local" as const,
  email: "",
  disabled: false,
  robot: false,
  created_at: "2026-01-01T00:00:00Z",
  last_login_at: null,
  roles: [{ id: 7, name: "maven-readers" }],
  lockout_enabled: false,
  locked: false,
  protected: false,
  token_count: 0,
};

// Retries off and no cache carried between tests: a retry would turn an
// expected failure into a timeout, and a shared cache would let one test's
// roles answer the next test's query.
function withQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

  return {
    queryClient,
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

describe("useRoleDetail", () => {
  beforeEach(() => {
    mockedRoles.listRoles.mockResolvedValue([role]);
    mockedUsers.listUsers.mockResolvedValue([member]);
  });

  test("resolves the role from the list and its members from the users", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.role?.name).toBe("maven-readers");
    expect(result.current.members).toHaveLength(1);
    expect(result.current.error).toBe("");
  });

  test("a user holding another role is not a member", async () => {
    mockedUsers.listUsers.mockResolvedValue([
      { ...member, roles: [{ id: 8, name: "other" }] },
    ]);
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.members).toEqual([]);
  });

  test("a non-numeric id fetches nothing", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(Number("not-an-id")), { wrapper });

    expect(mockedRoles.listRoles).not.toHaveBeenCalled();
    expect(mockedUsers.listUsers).not.toHaveBeenCalled();
    expect(result.current.role).toBeUndefined();
  });

  // The regression the error chaining invites: getErrorMessage always returns a
  // sentence, so combining sources with || would report a failure that never
  // happened.
  test("a healthy screen reports no error", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.error).toBe("");
  });

  test("a failing query surfaces its message", async () => {
    mockedRoles.listRoles.mockRejectedValue(new ApiError(500, "roles unavailable"));
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.error).toBe("roles unavailable"));
  });

  test("an action failure takes precedence over a query error", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    await act(async () => {
      result.current.runAction(Promise.reject(new ApiError(409, "role is managed")));
    });

    expect(result.current.error).toBe("role is managed");
  });

  test("running an action clears the previous failure", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRoleDetail(7), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    await act(async () => {
      result.current.runAction(Promise.reject(new ApiError(409, "role is managed")));
    });
    await act(async () => {
      result.current.runAction(Promise.resolve());
    });

    expect(result.current.error).toBe("");
  });
});

describe("role permission mutations", () => {
  beforeEach(() => {
    mockedRoles.listRoles.mockResolvedValue([role]);
    mockedUsers.listUsers.mockResolvedValue([member]);
    mockedRoles.createRolePermissions.mockResolvedValue({
      id: 4,
      repo_pattern: "npm-*",
      actions: ["read"],
    });
    mockedRoles.deleteRolePermissions.mockResolvedValue(undefined);
  });

  test("granting a permission refetches the role list", async () => {
    const { wrapper } = withQueryClient();
    const detail = renderHook(() => useRoleDetail(7), { wrapper });
    const mutation = renderHook(() => useAddRolePermissionMutation(), { wrapper });

    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({
        roleId: 7,
        permission: { repo_pattern: "npm-*", actions: ["read"] },
      });
    });

    expect(mockedRoles.createRolePermissions).toHaveBeenCalledWith({
      path: { id: 7 },
      body: { repo_pattern: "npm-*", actions: ["read"] },
    });
    // Without the invalidation the page would keep showing the old permission
    // set until a reload.
    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(2));
    expect(detail.result.current.role).toBeDefined();
  });

  test("revoking a permission sends both ids and refetches", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRoleDetail(7), { wrapper });
    const mutation = renderHook(() => useRemoveRolePermissionMutation(), { wrapper });

    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ roleId: 7, permissionId: 3 });
    });

    expect(mockedRoles.deleteRolePermissions).toHaveBeenCalledWith({
      path: { id: 7, permID: 3 },
    });
    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(2));
  });

  test("a failed grant leaves the cached roles alone", async () => {
    mockedRoles.createRolePermissions.mockRejectedValue(new ApiError(409, "managed role"));
    const { wrapper } = withQueryClient();
    renderHook(() => useRoleDetail(7), { wrapper });
    const mutation = renderHook(() => useAddRolePermissionMutation(), { wrapper });

    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(1));

    await act(async () => {
      await expect(
        mutation.result.current.mutateAsync({
          roleId: 7,
          permission: { repo_pattern: "npm-*", actions: ["read"] },
        }),
      ).rejects.toThrow("managed role");
    });

    expect(mockedRoles.listRoles).toHaveBeenCalledTimes(1);
  });
});
