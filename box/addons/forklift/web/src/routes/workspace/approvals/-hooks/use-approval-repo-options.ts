import { useQuery } from "@tanstack/react-query";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// The repository filter and the "add rule" modal both need the set of
// repositories an approval can belong to, plus their ids so the queue can link
// to them.
//
// Two sources, because listing repositories is admin-only and an approver need
// not be an admin. The pending-repos endpoint is open to anyone who can see the
// queue and carries names and ids as well, so a non-admin approver still gets a
// usable filter instead of an empty one.
//
// This replaces what the page used to do: read the repository names back out of
// whichever approval rows happened to be on screen. That was never the full
// set - it was one page of one filter - and it forced the list component to
// report its rows upward through a callback.
export function useApprovalRepoOptions() {
  const repositoriesQuery = useQuery({
    ...openApiQueryOptions.listRepositories(),
    meta: { suppressGlobalErrorToast: true },
  });
  const pendingReposQuery = useQuery({
    ...openApiQueryOptions.listApprovalsPendingRepos(),
    meta: { suppressGlobalErrorToast: true },
  });

  // Only proxy and hosted repositories can carry approvals; a group repository
  // has no upstream of its own to approve against.
  const repositories = (repositoriesQuery.data ?? []).filter(
    (repository) => repository.type === "proxy" || repository.type === "hosted",
  );
  const pendingRepos = pendingReposQuery.data?.repos ?? [];

  const idsByName: Record<string, number> = {};
  for (const repository of repositories) idsByName[repository.name] = repository.id;
  for (const pending of pendingRepos) {
    // The admin listing wins where both know the repository; the pending list's
    // id is absent for a non-admin, and 0 would produce a link to nowhere.
    if (pending.id && !idsByName[pending.repo_name]) idsByName[pending.repo_name] = pending.id;
  }

  const names = new Set([
    ...repositories.map((repository) => repository.name),
    ...pendingRepos.map((pending) => pending.repo_name),
  ]);

  return { idsByName, names: [...names].sort() };
}
