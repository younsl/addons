import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// The list endpoint returns more than the detail one: the aggregate counts
// this table shows are computed per listing and are absent everywhere else.
// RepositoryListItem is the document's name for that wider shape.
import type { RepositoryListItem } from "@/services/v1/openapi-types";

// useRepositoriesList fetches the directory and works out its shape: which rows
// are top level, which are nested under a group, and which groups are open.
export function useRepositoriesList() {
  // Groups are expanded by default, so what is tracked is the exception: the
  // ids a user has explicitly collapsed.
  //
  // The screen used to seed an "expanded" set from the fetch instead. Under
  // React Query that would re-open every group on each background refetch, and
  // a group created since would never appear open at all. Storing the negative
  // needs no seeding and has no such gap.
  const [collapsedIds, setCollapsedIds] = useState<Set<number>>(new Set());
  const repositoriesQuery = useQuery({
    ...openApiQueryOptions.listRepositories(),
    meta: { suppressGlobalErrorToast: true },
  });

  const repositories = repositoriesQuery.data ?? [];
  const byName: Record<string, RepositoryListItem> = Object.fromEntries(
    repositories.map((repository) => [repository.name, repository]),
  );

  // Names that belong to at least one group are shown only nested under their
  // group(s), never as a duplicate top-level row.
  const memberNames = new Set(
    repositories.flatMap((repository) =>
      repository.type === "group" ? repository.config.group?.members ?? [] : [],
    ),
  );

  return {
    byName,
    error: getErrorMessageIfAny(repositoriesQuery.error),
    isEmpty: !repositoriesQuery.isPending && repositories.length === 0,
    repositories,
    topLevel: repositories.filter((repository) => !memberNames.has(repository.name)),
    isExpanded: (id: number) => !collapsedIds.has(id),
    toggleGroup: (id: number) =>
      setCollapsedIds((current) => {
        const next = new Set(current);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        return next;
      }),
  };
}
