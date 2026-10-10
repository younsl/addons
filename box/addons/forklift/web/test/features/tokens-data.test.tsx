import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import * as tokensApi from "@/services/v1/tokens/api";
import * as usersApi from "@/services/v1/users/api";
import {
  useCreateTokenMutation,
  useRevokeTokenMutation,
  useTokensList,
  useUpdateTokenScopesMutation,
} from "@/routes/workspace/tokens/-hooks/use-token-mutations";

vi.mock("@/services/v1/tokens/api");
vi.mock("@/services/v1/users/api");

const mockedTokens = vi.mocked(tokensApi);
const mockedUsers = vi.mocked(usersApi);

const token = {
  id: 11,
  name: "ci",
  description: "build agent",
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
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

describe("the current user's tokens", () => {
  beforeEach(() => {
    mockedTokens.listTokens.mockResolvedValue([token]);
    mockedTokens.deleteToken.mockResolvedValue(undefined);
    mockedTokens.updateToken.mockResolvedValue(undefined);
  });

  test("revoking refetches the list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useTokensList(), { wrapper });
    const mutation = renderHook(() => useRevokeTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync(11);
    });

    expect(mockedTokens.deleteToken).toHaveBeenCalledWith({ path: { id: 11 } });
    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(2));
  });

  test("editing scopes sends the full replacement list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useTokensList(), { wrapper });
    const mutation = renderHook(() => useUpdateTokenScopesMutation(), { wrapper });

    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({
        tokenId: 11,
        scopes: [{ repo_pattern: "npm-*", actions: ["read", "write"] }],
      });
    });

    expect(mockedTokens.updateToken).toHaveBeenCalledWith({
      path: { id: 11 },
      body: { scopes: [{ repo_pattern: "npm-*", actions: ["read", "write"] }] },
    });
    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(2));
  });
});

// The same create page serves two endpoints, and which lists go stale differs.
// Getting this wrong is invisible until an admin issues a token and the user's
// detail page still shows the old count.
describe("creating a token for one identity or another", () => {
  beforeEach(() => {
    mockedTokens.listTokens.mockResolvedValue([token]);
    mockedTokens.listUserTokens.mockResolvedValue([token]);
    mockedUsers.listUsers.mockResolvedValue([]);
    mockedTokens.createTokens.mockResolvedValue({ token: "secret" });
    mockedTokens.createUserTokens.mockResolvedValue({ token: "secret" });
  });

  const body = {
    name: "ci",
    description: "build agent",
    scopes: [{ repo_pattern: "maven-*", actions: ["read" as const] }],
    expires_in: "24h",
  };

  test("self-service posts to /tokens and refreshes only that list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useTokensList(), { wrapper });
    const mutation = renderHook(() => useCreateTokenMutation(), { wrapper });

    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ forUserId: null, body });
    });

    expect(mockedTokens.createTokens).toHaveBeenCalledWith({ body });
    expect(mockedTokens.createUserTokens).not.toHaveBeenCalled();
    await waitFor(() => expect(mockedTokens.listTokens).toHaveBeenCalledTimes(2));
  });

  test("issuing for a user posts to that user's endpoint and refreshes the directory", async () => {
    const { wrapper } = withQueryClient();
    const mutation = renderHook(() => useCreateTokenMutation(), { wrapper });

    await act(async () => {
      await mutation.result.current.mutateAsync({ forUserId: 4, body });
    });

    expect(mockedTokens.createUserTokens).toHaveBeenCalledWith({ path: { id: 4 }, body });
    expect(mockedTokens.createTokens).not.toHaveBeenCalled();
  });
});
