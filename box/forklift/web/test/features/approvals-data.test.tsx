import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import * as approvalsApi from "@/services/v1/approvals/api";
import * as repositoriesApi from "@/services/v1/repositories/api";
import * as usersApi from "@/services/v1/users/api";
import {
  useApproveAllPendingMutation,
  useCreateApprovalMutation,
  useCreateVersionDenyMutation,
  useDecideApprovalMutation,
  useRemoveVersionDenyMutation,
} from "@/routes/workspace/approvals/-hooks/use-approval-mutations";
import { useApprovalQueue } from "@/routes/workspace/approvals/-hooks/use-approval-queue";
import { useApprovalRepoOptions } from "@/routes/workspace/approvals/-hooks/use-approval-repo-options";

vi.mock("@/services/v1/approvals/api");
vi.mock("@/services/v1/repositories/api");
vi.mock("@/services/v1/users/api");

const mockedApprovals = vi.mocked(approvalsApi);
const mockedRepositories = vi.mocked(repositoriesApi);
const mockedUsers = vi.mocked(usersApi);

const approval = {
  id: 3,
  repo_name: "npm-proxy",
  package: "lodash",
  status: "pending" as const,
  request_count: 2,
  requested_by: "alice",
  first_requested_at: "2026-01-01T00:00:00Z",
  last_requested_at: "2026-01-02T00:00:00Z",
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

beforeEach(() => {
  mockedApprovals.listApprovals.mockResolvedValue({ approvals: [approval], count: 1 });
  mockedApprovals.getApprovalsCount.mockResolvedValue({ count: 7 });
  mockedApprovals.listApprovalsPendingRepos.mockResolvedValue({ repos: [] });
  mockedApprovals.listVersionDenies.mockResolvedValue({ denies: [], count: 0 });
  mockedApprovals.postApproveApproval.mockResolvedValue(approval);
  mockedApprovals.postRejectApproval.mockResolvedValue(approval);
  mockedApprovals.createApprovals.mockResolvedValue(approval);
  mockedApprovals.createVersionDenies.mockResolvedValue({
    id: 1,
    repo_name: "npm-proxy",
    package: "lodash",
    version: "4.17.20",
    reason: "",
    created_by: "admin",
    created_at: "2026-01-01T00:00:00Z",
  });
  mockedApprovals.deleteVersionDeny.mockResolvedValue(undefined);
  mockedApprovals.postApproveAllApprovals.mockResolvedValue({ approved: 4 });
  mockedRepositories.listRepositories.mockResolvedValue([]);
  mockedUsers.listUsers.mockResolvedValue([]);
});

describe("useApprovalQueue", () => {
  test("sends the filters as query parameters and drops the empty ones", async () => {
    const { wrapper } = withQueryClient();
    renderHook(
      () => useApprovalQueue({ repo: "npm-proxy", status: "pending", page: 2, q: "", regex: false }),
      { wrapper },
    );

    await waitFor(() =>
      expect(mockedApprovals.listApprovals).toHaveBeenCalledWith(
        {
          query: {
            repo: "npm-proxy",
            status: "pending",
            // Empty search and unset regex must be omitted, not sent as "" and
            // false - the server treats a present q as a filter.
            q: undefined,
            regex: undefined,
            limit: 50,
            offset: 100,
          },
        },
        expect.anything(),
      ),
    );
  });

  test("the pending count is scoped to the same repository, not to the page", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(
      () => useApprovalQueue({ repo: "npm-proxy", status: "approved", page: 0, q: "", regex: false }),
      { wrapper },
    );

    await waitFor(() => expect(result.current.pendingCount).toBe(7));
    // Status "approved" is what the table shows; the count is always pending,
    // because that is what the bulk-approve button acts on.
    expect(mockedApprovals.getApprovalsCount).toHaveBeenCalledWith(
      { query: { repo: "npm-proxy", status: "pending" } },
      expect.anything(),
    );
    // Seven pending while one row is displayed: the count is not the page.
    expect(result.current.rows).toHaveLength(1);
  });
});

// A decision changes which of the pending/approved/rejected views a row belongs
// to. The queue is keyed by its filters, so invalidating one exact key would
// leave every other filter showing the row in its old state.
describe("a decision invalidates every filter of the queue", () => {
  test.each([
    ["approve", () => mockedApprovals.postApproveApproval],
    ["reject", () => mockedApprovals.postRejectApproval],
  ] as const)("%s posts a note and refetches", async (decision, getSpy) => {
    const { wrapper } = withQueryClient();
    renderHook(
      () => useApprovalQueue({ repo: "", status: "pending", page: 0, q: "", regex: false }),
      { wrapper },
    );
    const mutation = renderHook(() => useDecideApprovalMutation(), { wrapper });

    await waitFor(() => expect(mockedApprovals.listApprovals).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ approvalId: 3, decision, note: "ok" });
    });

    expect(getSpy()).toHaveBeenCalledWith({ path: { id: 3 }, body: { note: "ok" } });
    await waitFor(() => expect(mockedApprovals.listApprovals).toHaveBeenCalledTimes(2));
  });

  test("a queue on a different filter is refetched too", async () => {
    const { queryClient, wrapper } = withQueryClient();
    // Two views of the queue, differing only in status - as the page and the
    // repository tab would.
    renderHook(
      () => useApprovalQueue({ repo: "", status: "pending", page: 0, q: "", regex: false }),
      { wrapper },
    );
    renderHook(
      () => useApprovalQueue({ repo: "npm-proxy", status: "approved", page: 0, q: "", regex: false }),
      { wrapper },
    );
    const mutation = renderHook(() => useDecideApprovalMutation(), { wrapper });

    await waitFor(() => expect(mockedApprovals.listApprovals).toHaveBeenCalledTimes(2));

    await act(async () => {
      await mutation.result.current.mutateAsync({ approvalId: 3, decision: "approve", note: "" });
    });

    await waitFor(() => expect(mockedApprovals.listApprovals).toHaveBeenCalledTimes(4));
    // The prefix that made both stale.
    expect(
      queryClient.getQueryCache().findAll({
        queryKey: operationKeyPrefix(openApiQueryKeys.listApprovals()),
      }),
    ).toHaveLength(2);
  });

  test("the sidebar's pending count is refetched as well", async () => {
    const { wrapper } = withQueryClient();
    renderHook(
      () => useApprovalQueue({ repo: "", status: "pending", page: 0, q: "", regex: false }),
      { wrapper },
    );
    const mutation = renderHook(() => useDecideApprovalMutation(), { wrapper });

    await waitFor(() => expect(mockedApprovals.getApprovalsCount).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ approvalId: 3, decision: "approve", note: "" });
    });

    await waitFor(() => expect(mockedApprovals.getApprovalsCount).toHaveBeenCalledTimes(2));
  });
});

describe("rules recorded ahead of demand", () => {
  test("allow and block become approval decisions", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useCreateApprovalMutation(), { wrapper });

    await act(async () => {
      await result.current.mutateAsync({
        repo: "npm-proxy",
        package: "lodash",
        status: "approved",
        note: "vetted",
      });
    });

    expect(mockedApprovals.createApprovals).toHaveBeenCalledWith({
      body: { repo: "npm-proxy", package: "lodash", status: "approved", note: "vetted" },
    });
  });

  // Same form, different endpoint: a version block is not an approval decision,
  // it is a deny that overrides one.
  test("a version block goes to the deny list instead", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useCreateVersionDenyMutation(), { wrapper });

    await act(async () => {
      await result.current.mutateAsync({
        repo: "npm-proxy",
        package: "lodash",
        version: "4.17.20",
        reason: "IOC",
      });
    });

    expect(mockedApprovals.createVersionDenies).toHaveBeenCalledWith({
      body: { repo: "npm-proxy", package: "lodash", version: "4.17.20", reason: "IOC" },
    });
    expect(mockedApprovals.createApprovals).not.toHaveBeenCalled();
  });

  test("removing a deny refetches the deny list", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useRemoveVersionDenyMutation(), { wrapper });

    await act(async () => {
      await result.current.mutateAsync(1);
    });

    expect(mockedApprovals.deleteVersionDeny).toHaveBeenCalledWith({ path: { id: 1 } });
  });
});

test("bulk approve carries the clean-only decision", async () => {
  const { wrapper } = withQueryClient();
  const { result } = renderHook(() => useApproveAllPendingMutation(), { wrapper });

  await act(async () => {
    await result.current.mutateAsync({ repo: "npm-proxy", note: "quarterly", cleanOnly: true });
  });

  expect(mockedApprovals.postApproveAllApprovals).toHaveBeenCalledWith({
    body: { repo: "npm-proxy", note: "quarterly", clean_only: true },
  });
});

describe("useApprovalRepoOptions", () => {
  test("merges the admin listing with the pending repos", async () => {
    mockedRepositories.listRepositories.mockResolvedValue([
      { id: 1, name: "npm-proxy", type: "proxy", format: "npm" },
      // A group repository has no upstream of its own to approve against.
      { id: 2, name: "npm-group", type: "group", format: "npm" },
    ] as never);
    mockedApprovals.listApprovalsPendingRepos.mockResolvedValue({
      repos: [
        { repo_name: "npm-proxy", format: "npm", type: "proxy", pending: 3, clean: 1, id: 1 },
        { repo_name: "pypi-proxy", format: "pypi", type: "proxy", pending: 2, clean: 0, id: 5 },
      ],
    });
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useApprovalRepoOptions(), { wrapper });

    await waitFor(() => expect(result.current.names).toEqual(["npm-proxy", "pypi-proxy"]));
    expect(result.current.idsByName).toEqual({ "npm-proxy": 1, "pypi-proxy": 5 });
  });

  // A non-admin approver cannot list repositories at all, and would previously
  // have been left with whatever names one page of rows happened to mention.
  test("a failed repository listing still yields the pending repos", async () => {
    mockedRepositories.listRepositories.mockRejectedValue(new Error("forbidden"));
    mockedApprovals.listApprovalsPendingRepos.mockResolvedValue({
      repos: [{ repo_name: "pypi-proxy", format: "pypi", type: "proxy", pending: 2, clean: 0, id: 0 }],
    });
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useApprovalRepoOptions(), { wrapper });

    await waitFor(() => expect(result.current.names).toEqual(["pypi-proxy"]));
    // id 0 is "not known", not repository zero: it must not produce a link.
    expect(result.current.idsByName).toEqual({});
  });
});
