import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createMemoryHistory, createRouter } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";

import { routeTree } from "@/generated/route-tree.gen";

// Pins the redirect loop that made every "turns away" screen log "Maximum
// update depth exceeded": a permission gate rendering the router's <Navigate>
// inline navigated again on every render while the router was transitioning
// away, 52 times for one redirect. The count is the assertion - the redirect
// still lands, so nothing else here would notice.
//
// /access/roles is one such gate: it is for admins and auditors, and turns
// everyone else back to the repository directory.

const repositories = [
  {
    id: 1,
    name: "maven-central",
    format: "maven",
    type: "proxy",
    upstream_url: "https://repo1.maven.org",
    config: { cache: { enabled: true, max_size_bytes: 0 }, age_policy: {}, approval: {} },
  },
];

function responseBody(path: string): unknown {
  if (path.startsWith("/api/v1/me")) {
    return { authenticated: true, username: "reader", admin: false };
  }
  if (path.includes("/upstream-health")) {
    return { applicable: true, reachable: true, status: 200, latency_ms: 12 };
  }
  if (path.startsWith("/api/v1/repositories")) return repositories;
  return {};
}

beforeEach(() => {
  vi.stubGlobal("fetch", (input: RequestInfo | URL) =>
    Promise.resolve(
      new Response(JSON.stringify(responseBody(String(input))), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    ),
  );
});

test("a permission gate redirects once, not once per render", async () => {
  const router = createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: ["/access/roles"] }),
  });
  const navigate = router.navigate.bind(router);
  const calls: unknown[] = [];
  router.navigate = ((...args: Parameters<typeof navigate>) => {
    calls.push(args[0]);
    return navigate(...args);
  }) as typeof router.navigate;

  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router as never} />
    </QueryClientProvider>,
  );

  await waitFor(() => expect(screen.getByText("maven-central")).toBeInTheDocument());

  expect(calls).toHaveLength(1);
});
