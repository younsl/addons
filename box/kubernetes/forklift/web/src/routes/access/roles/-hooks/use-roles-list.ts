import { useQuery } from "@tanstack/react-query";

import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// useRolesList is the whole data layer of the roles directory. The screen calls
// it and knows neither the endpoint nor the query key - both come from the
// generated options, spread whole so the queryFn and its types come along.
export function useRolesList() {
  return useQuery({
    ...openApiQueryOptions.listRoles(),
    // The page renders the failure inline, above the table.
    meta: { suppressGlobalErrorToast: true },
  });
}
