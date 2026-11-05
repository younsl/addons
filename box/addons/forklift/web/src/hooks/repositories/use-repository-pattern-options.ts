import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// The pattern that grants every repository. It is not a repository name, so it
// leads the list rather than sorting among them.
export const ALL_REPOSITORIES_PATTERN = "*";

// useRepositoryPatternOptions feeds the pattern combobox wherever a repository
// glob is entered - role permissions and token scopes both. The names are only
// an autocomplete aid: a pattern that matches nothing today is still valid, so
// a failed fetch degrades to "*" alone instead of blocking the form. That is
// why the error is swallowed rather than surfaced.
export function useRepositoryPatternOptions() {
  const repositoriesQuery = useQuery({
    ...openApiQueryOptions.listRepositoryNames(),
    meta: { suppressGlobalErrorToast: true },
  });
  const repositories = repositoriesQuery.data;

  const options = useMemo(
    () => [
      ALL_REPOSITORIES_PATTERN,
      ...(repositories ?? []).map((repository) => repository.name),
    ],
    [repositories],
  );

  // Rendered next to each name in the dropdown, so "maven-releases" reads as
  // the hosted Maven repository rather than as a bare string.
  const types = useMemo(
    () =>
      Object.fromEntries(
        (repositories ?? []).map((repository) => [
          repository.name,
          `${repository.format} · ${repository.type}`,
        ]),
      ),
    [repositories],
  );

  return { options, types };
}
