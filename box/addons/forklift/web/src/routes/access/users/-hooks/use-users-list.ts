import { useQuery } from "@tanstack/react-query";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// useUsersList is the whole data layer of the user directory.
export function useUsersList() {
  return useQuery({
    ...openApiQueryOptions.listUsers(),
    // The page renders the failure inline, above the table.
    meta: { suppressGlobalErrorToast: true },
  });
}
