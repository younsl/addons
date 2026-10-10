import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import * as approvalsApi from "@/services/v1/approvals/api";
import * as notificationApi from "@/services/v1/notification/api";
import * as repositoriesApi from "@/services/v1/repositories/api";
import * as rolesApi from "@/services/v1/roles/api";
import * as tokensApi from "@/services/v1/tokens/api";
import * as usersApi from "@/services/v1/users/api";

import { useAddRolePermissionMutation } from "@/routes/access/roles/-hooks/use-role-mutations";
import { useRevokeUserTokenMutation } from "@/routes/access/users/-hooks/use-user-mutations";
import { useDecideApprovalMutation } from "@/routes/workspace/approvals/-hooks/use-approval-mutations";
import { useUpdateRepositorySecurityMutation } from "@/routes/workspace/repositories/-hooks/use-repository-mutations";
import {
  useCreateTokenMutation,
  useRevokeTokenMutation,
} from "@/routes/workspace/tokens/-hooks/use-token-mutations";

vi.mock("@/services/v1/approvals/api");
vi.mock("@/services/v1/notification/api");
vi.mock("@/services/v1/repositories/api");
vi.mock("@/services/v1/roles/api");
vi.mock("@/services/v1/tokens/api");
vi.mock("@/services/v1/users/api");

const mockedApprovals = vi.mocked(approvalsApi);
const mockedNotification = vi.mocked(notificationApi);
const mockedRepositories = vi.mocked(repositoriesApi);
const mockedRoles = vi.mocked(rolesApi);
const mockedTokens = vi.mocked(tokensApi);
const mockedUsers = vi.mocked(usersApi);

function withQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

  return {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

// The screens these stand in for: the repository directory, the notification
// receiver list, and the repository detail's Permissions tab.
const useRepositoryList = () => useQuery(openApiQueryOptions.listRepositories());
const useReceiverList = () => useQuery(openApiQueryOptions.listNotificationReceivers());
const useRepoPermissions = () =>
  useQuery(openApiQueryOptions.listRepositoryPermissions({ path: { id: 6 } }));
const useRepoTokens = () =>
  useQuery(openApiQueryOptions.listRepositoryTokens({ path: { id: 6 } }));

beforeEach(() => {
  mockedRepositories.listRepositories.mockResolvedValue([]);
  mockedRepositories.listRepositoryPermissions.mockResolvedValue([]);
  mockedRepositories.listRepositoryTokens.mockResolvedValue([]);
  mockedRepositories.updateRepositorySecurity.mockResolvedValue({} as never);
  mockedNotification.listNotificationReceivers.mockResolvedValue([]);
  mockedRoles.createRolePermissions.mockResolvedValue({} as never);
  mockedTokens.deleteToken.mockResolvedValue(undefined);
  mockedTokens.createTokens.mockResolvedValue({ token: "secret" });
  mockedTokens.createUserTokens.mockResolvedValue({ token: "secret" });
  mockedTokens.deleteUserTokens.mockResolvedValue(undefined);
  mockedTokens.listUserTokens.mockResolvedValue([]);
  mockedUsers.listUsers.mockResolvedValue([]);
  mockedApprovals.postApproveApproval.mockResolvedValue({} as never);
});

// Each of these was a real staleness bug: the write changed a number or a list
// that a *different* screen renders, and nothing told that screen to look again.
// They are grouped here rather than in their domain files because the whole
// class is easy to reintroduce one mutation at a time.
describe("a write refreshes every screen that shows what it changed", () => {
  test("deciding an approval refreshes the repository directory's pending badge", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepositoryList(), { wrapper });
    const mutation = renderHook(() => useDecideApprovalMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.listRepositories).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({
        approvalId: 3,
        decision: "approve",
        note: "",
      });
    });

    // RepositoryListItem.pending_approval_count feeds the yellow dashed badge.
    await waitFor(() => expect(mockedRepositories.listRepositories).toHaveBeenCalledTimes(2));
  });

  test("saving a repository's security refreshes the receiver list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useReceiverList(), { wrapper });
    const mutation = renderHook(() => useUpdateRepositorySecurityMutation(), { wrapper });

    await waitFor(() =>
      expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(1),
    );

    await act(async () => {
      await mutation.result.current.mutateAsync({ repositoryId: 6, body: {} as never });
    });

    // The body carries notify.receivers, and Receiver.repositories is what
    // gates the receiver's delete button.
    await waitFor(() =>
      expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(2),
    );
  });

  test("granting a role permission refreshes the repository Permissions tab", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepoPermissions(), { wrapper });
    const mutation = renderHook(() => useAddRolePermissionMutation(), { wrapper });

    await waitFor(() =>
      expect(mockedRepositories.listRepositoryPermissions).toHaveBeenCalledTimes(1),
    );

    await act(async () => {
      await mutation.result.current.mutateAsync({
        roleId: 7,
        permission: { repo_pattern: "maven-*", actions: ["read"] },
      });
    });

    // A pattern covers repositories this hook cannot enumerate, so every
    // repository's tab is invalidated.
    await waitFor(() =>
      expect(mockedRepositories.listRepositoryPermissions).toHaveBeenCalledTimes(2),
    );
  });

  test("revoking your own token refreshes the repository Permissions tab", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepoTokens(), { wrapper });
    const mutation = renderHook(() => useRevokeTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync(11);
    });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(2));
  });

  test("revoking another user's token refreshes it too", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepoTokens(), { wrapper });
    const mutation = renderHook(() => useRevokeUserTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ userId: 4, tokenId: 11 });
    });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(2));
  });

  // Both create paths scope a token to repositories, so neither may skip it.
  test.each([
    ["self-service", null],
    ["issued for a user", 4],
  ])("creating a token (%s) refreshes the repository Permissions tab", async (_name, forUserId) => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepoTokens(), { wrapper });
    const mutation = renderHook(() => useCreateTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({
        forUserId: forUserId as number | null,
        body: {
          name: "ci",
          description: "d",
          scopes: [{ repo_pattern: "maven-*", actions: ["read"] }],
          expires_in: "24h",
        },
      });
    });

    await waitFor(() => expect(mockedRepositories.listRepositoryTokens).toHaveBeenCalledTimes(2));
  });
});
