import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { ApiError } from "@/lib/http/error/api-error";
import * as rolesApi from "@/services/v1/roles/api";
import * as tokensApi from "@/services/v1/tokens/api";
import * as usersApi from "@/services/v1/users/api";
import { useUserDetail } from "@/routes/access/users/-hooks/use-user-detail";
import {
  useAssignUserRoleMutation,
  useImpersonateUserMutation,
  useRevokeUserTokenMutation,
  useUpdateUserMutation,
} from "@/routes/access/users/-hooks/use-user-mutations";

vi.mock("@/services/v1/roles/api");
vi.mock("@/services/v1/tokens/api");
vi.mock("@/services/v1/users/api");

const mockedRoles = vi.mocked(rolesApi);
const mockedTokens = vi.mocked(tokensApi);
const mockedUsers = vi.mocked(usersApi);

const user = {
  id: 4,
  username: "alice",
  source: "local" as const,
  email: "",
  disabled: false,
  robot: false,
  created_at: "2026-01-01T00:00:00Z",
  last_login_at: null,
  roles: [{ id: 7, name: "maven-readers" }],
  lockout_enabled: true,
  locked: false,
  protected: false,
  token_count: 1,
};

const role = {
  id: 7,
  name: "maven-readers",
  description: "",
  created_at: "2026-01-01T00:00:00Z",
  managed: false,
  permissions: [],
  user_count: 1,
};

const token = {
  id: 11,
  name: "ci",
  description: "",
  scopes_json: '[{"repo_pattern":"maven-*","actions":["read"]}]',
  expires_at: null,
  last_used_at: null,
  created_at: "2026-01-01T00:00:00Z",
};

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

describe("useUserDetail", () => {
  beforeEach(() => {
    mockedUsers.listUsers.mockResolvedValue([user]);
    mockedRoles.listRoles.mockResolvedValue([role]);
    mockedTokens.listUserTokens.mockResolvedValue([token]);
  });

  test("gathers the user, the assignable roles and the user's tokens", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useUserDetail(4), { wrapper });

    await waitFor(() => expect(result.current.isLoading).toBe(false));
    expect(result.current.user?.username).toBe("alice");
    expect(result.current.roles).toHaveLength(1);
    expect(result.current.tokens).toHaveLength(1);
    expect(result.current.error).toBe("");
  });

  test("the token query is keyed to the user in the URL", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useUserDetail(4), { wrapper });

    await waitFor(() =>
      expect(mockedTokens.listUserTokens).toHaveBeenCalledWith(
        { path: { id: 4 } },
        expect.anything(),
      ),
    );
  });

  test("a non-numeric id fetches nothing", () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useUserDetail(Number("nope")), { wrapper });

    expect(mockedUsers.listUsers).not.toHaveBeenCalled();
    expect(mockedTokens.listUserTokens).not.toHaveBeenCalled();
  });

  test("only one of the three failing is enough to report", async () => {
    mockedTokens.listUserTokens.mockRejectedValue(new ApiError(500, "tokens unavailable"));
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useUserDetail(4), { wrapper });

    await waitFor(() => expect(result.current.error).toBe("tokens unavailable"));
    // The user still resolved, so the page renders with the token panel empty
    // rather than showing nothing at all.
    expect(result.current.user?.username).toBe("alice");
  });
});

describe("user mutations", () => {
  beforeEach(() => {
    mockedUsers.listUsers.mockResolvedValue([user]);
    mockedRoles.listRoles.mockResolvedValue([role]);
    mockedTokens.listUserTokens.mockResolvedValue([token]);
    mockedUsers.updateUser.mockResolvedValue(user);
    mockedUsers.createUserRoles.mockResolvedValue(undefined);
    mockedTokens.deleteUserTokens.mockResolvedValue(undefined);
    mockedUsers.postImpersonateUser.mockResolvedValue({
      username: "alice",
      source: "local",
      impersonator: "admin",
    });
  });

  // Password reset, disable, lockout and unlock are one endpoint with different
  // fields. This pins the body each control sends.
  test.each([
    ["resets a password", { password: "s3cret" }],
    ["disables an account", { disabled: true }],
    ["turns lockout off", { lockout_enabled: false }],
    ["unlocks", { unlock: true }],
  ])("%s through PUT /users/{id}", async (_name, body) => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useUpdateUserMutation(), { wrapper });

    await act(async () => {
      await result.current.mutateAsync({ userId: 4, body });
    });

    expect(mockedUsers.updateUser).toHaveBeenCalledWith({ path: { id: 4 }, body });
  });

  test("assigning a role refetches both the users and the roles", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useUserDetail(4), { wrapper });
    const mutation = renderHook(() => useAssignUserRoleMutation(), { wrapper });

    await waitFor(() => expect(mockedUsers.listUsers).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ userId: 4, roleId: 9 });
    });

    expect(mockedUsers.createUserRoles).toHaveBeenCalledWith({
      path: { id: 4 },
      body: { role_id: 9 },
    });
    // The role list carries user_count, so it is stale as well.
    await waitFor(() => expect(mockedUsers.listUsers).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(mockedRoles.listRoles).toHaveBeenCalledTimes(2));
  });

  test("revoking a token refetches that user's tokens and the directory", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useUserDetail(4), { wrapper });
    const mutation = renderHook(() => useRevokeUserTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedTokens.listUserTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ userId: 4, tokenId: 11 });
    });

    expect(mockedTokens.deleteUserTokens).toHaveBeenCalledWith({
      path: { id: 4, tokenID: 11 },
    });
    await waitFor(() => expect(mockedTokens.listUserTokens).toHaveBeenCalledTimes(2));
    // The directory shows a per-user token count.
    await waitFor(() => expect(mockedUsers.listUsers).toHaveBeenCalledTimes(2));
  });

  // The cookie changes underneath, so refreshing a query would read the new
  // identity into a cache built for the old one. The caller reloads instead.
  test("impersonating invalidates nothing", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useUserDetail(4), { wrapper });
    const mutation = renderHook(() => useImpersonateUserMutation(), { wrapper });

    await waitFor(() => expect(mockedUsers.listUsers).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ userId: 4, reason: "support ticket 42" });
    });

    expect(mockedUsers.postImpersonateUser).toHaveBeenCalledWith({
      path: { id: 4 },
      body: { reason: "support ticket 42" },
    });
    expect(mockedUsers.listUsers).toHaveBeenCalledTimes(1);
  });
});
