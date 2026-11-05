import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import * as repositoriesApi from "@/services/v1/repositories/api";
import { useRepositoryDraft } from "@/routes/workspace/repositories/-hooks/use-repository-draft";
import {
  useDeleteRepositoryMutation,
  useSetRepositoryDisabledMutation,
  useUpdateRepositorySecurityMutation,
} from "@/routes/workspace/repositories/-hooks/use-repository-mutations";
import { useUpstreamCheck } from "@/routes/workspace/repositories/-hooks/use-upstream-check";

import type { Repository } from "@/services/v1/openapi-types";

vi.mock("@/services/v1/repositories/api");

const mockedRepositories = vi.mocked(repositoriesApi);

const repository = {
  id: 6,
  name: "maven-hosted",
  format: "maven",
  type: "hosted",
  upstream_url: "",
  disabled: false,
  seeded: false,
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  capabilities: { read: true, write: true, delete: true, upload: true },
  publish_methods: ["mvn"],
  config: {},
} as unknown as Repository;

// A stand-in for the detail page's own query, so the invalidation tests have
// something to observe.
function useRepositoryDetailQuery() {
  return useQuery(openApiQueryOptions.getRepository({ path: { id: 6 } }));
}

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

beforeEach(() => {
  mockedRepositories.listRepositories.mockResolvedValue([]);
  mockedRepositories.getRepository.mockResolvedValue(repository);
  mockedRepositories.updateRepositorySecurity.mockResolvedValue(repository);
  mockedRepositories.postDisabledRepository.mockResolvedValue(repository);
  mockedRepositories.deleteRepository.mockResolvedValue(undefined);
  mockedRepositories.postCheckUpstreamRepositories.mockResolvedValue({
    applicable: true,
    reachable: true,
    status: 200,
    latency_ms: 12,
  });
});

// The settings and security tabs edit a repository field by field. Under the
// old code the page fetched once, so seeding the form from the response was
// safe. Under React Query the value can arrive again at any moment.
describe("useRepositoryDraft", () => {
  test("starts from the fetched repository", () => {
    const { result } = renderHook(() => useRepositoryDraft(repository));

    expect(result.current[0].name).toBe("maven-hosted");
  });

  test("a refetch that changed nothing does not discard an edit in progress", () => {
    const { result, rerender } = renderHook(({ repo }) => useRepositoryDraft(repo), {
      initialProps: { repo: repository },
    });

    act(() => result.current[1]({ ...result.current[0], upstream_url: "https://typed.test" }));
    // Same updated_at: the server's copy has not moved, so neither should the
    // draft. A fresh object identity alone must not be enough to re-seed.
    rerender({ repo: { ...repository } });

    expect(result.current[0].upstream_url).toBe("https://typed.test");
  });

  test("a save or an edit elsewhere does re-seed it", () => {
    const { result, rerender } = renderHook(({ repo }) => useRepositoryDraft(repo), {
      initialProps: { repo: repository },
    });

    act(() => result.current[1]({ ...result.current[0], upstream_url: "https://typed.test" }));
    rerender({ repo: { ...repository, updated_at: "2026-02-02T00:00:00Z" } as Repository });

    expect(result.current[0].upstream_url).toBe("");
  });

  test("navigating to another repository re-seeds it too", () => {
    const { result, rerender } = renderHook(({ repo }) => useRepositoryDraft(repo), {
      initialProps: { repo: repository },
    });

    rerender({ repo: { ...repository, id: 7, name: "npm-hosted" } as Repository });

    expect(result.current[0].name).toBe("npm-hosted");
  });
});

describe("repository mutations", () => {
  test("the security save sends only the policy sections", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useUpdateRepositorySecurityMutation(), { wrapper });

    await act(async () => {
      await result.current.mutateAsync({
        repositoryId: 6,
        body: {
          config: {
            age_policy: { enabled: false },
            approval: { enabled: true, mode: "enforce" },
          },
        } as never,
      });
    });

    const [call] = mockedRepositories.updateRepositorySecurity.mock.calls[0];
    // upstream_auth must not ride along: this tab is reachable by a security
    // engineer with no rights over the upstream or its credentials.
    expect(call.body.config).not.toHaveProperty("upstream_auth");
    expect(call.path).toEqual({ id: 6 });
  });

  test("taking a repository offline refetches its detail", async () => {
    const { wrapper } = withQueryClient();
    const detail = renderHook(
      () => useRepositoryDetailQuery(),
      { wrapper },
    );
    const mutation = renderHook(() => useSetRepositoryDisabledMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.getRepository).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync({ repositoryId: 6, disabled: true });
    });

    expect(mockedRepositories.postDisabledRepository).toHaveBeenCalledWith({
      path: { id: 6 },
      body: { disabled: true },
    });
    await waitFor(() => expect(mockedRepositories.getRepository).toHaveBeenCalledTimes(2));
    expect(detail.result.current).toBeDefined();
  });

  // Refetching a deleted repository's detail would only produce a 404.
  test("deleting does not refetch the deleted repository", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useRepositoryDetailQuery(), { wrapper });
    const mutation = renderHook(() => useDeleteRepositoryMutation(), { wrapper });

    await waitFor(() => expect(mockedRepositories.getRepository).toHaveBeenCalledTimes(1));

    await act(async () => {
      await mutation.result.current.mutateAsync(6);
    });

    expect(mockedRepositories.deleteRepository).toHaveBeenCalledWith({ path: { id: 6 } });
    // The directory is invalidated, but nothing here observes it, so only the
    // absence of a detail refetch is checkable - which is the point.
    expect(mockedRepositories.getRepository).toHaveBeenCalledTimes(1);
  });
});

describe("useUpstreamCheck", () => {
  test("does not probe until the URL stops changing", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { wrapper } = withQueryClient();
    const { result, rerender } = renderHook(
      ({ url }) => useUpstreamCheck({ url, auth: {}, enabled: true }),
      // Starts empty, as the create form does. A URL already present on mount
      // is probed at once and deliberately not debounced: there is nothing
      // being typed to wait for.
      { wrapper, initialProps: { url: "" } },
    );

    rerender({ url: "https://repo1" });
    rerender({ url: "https://repo1.maven" });
    rerender({ url: "https://repo1.maven.org" });
    expect(mockedRepositories.postCheckUpstreamRepositories).not.toHaveBeenCalled();
    // The hint says "checking" across the debounce window, not only during the
    // request - otherwise it blanks out between the last keystroke and the probe.
    expect(result.current.isChecking).toBe(true);

    await act(async () => { await vi.advanceTimersByTimeAsync(600); });

    expect(mockedRepositories.postCheckUpstreamRepositories).toHaveBeenCalledTimes(1);
    vi.useRealTimers();
  });

  test("probes nothing for a non-proxy repository", () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(
      () => useUpstreamCheck({ url: "https://repo1.maven.org", auth: {}, enabled: false }),
      { wrapper },
    );

    expect(mockedRepositories.postCheckUpstreamRepositories).not.toHaveBeenCalled();
    expect(result.current.isChecking).toBe(false);
  });

  // A probe that could not be made at all is not the same as an upstream that
  // answered "no": the difference is whether the URL is wrong or we are.
  test("a failed probe reports no result rather than unreachable", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mockedRepositories.postCheckUpstreamRepositories.mockRejectedValue(new Error("network"));
    const { wrapper } = withQueryClient();
    const { result } = renderHook(
      () => useUpstreamCheck({ url: "https://repo1.maven.org", auth: {}, enabled: true }),
      { wrapper },
    );

    await act(async () => { await vi.advanceTimersByTimeAsync(600); });
    await waitFor(() => expect(result.current.isChecking).toBe(false));

    expect(result.current.health).toBeNull();
    vi.useRealTimers();
  });
});

