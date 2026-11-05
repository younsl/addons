import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { APPROVALS_PAGE_SIZE } from "@/routes/workspace/approvals/-hooks/use-approval-queue";

export function useVersionDenies({ repo, offset }: { repo: string; offset: number }) {
  const deniesQuery = useQuery({
    ...openApiQueryOptions.listVersionDenies({
      query: {
        repo: repo || undefined,
        limit: APPROVALS_PAGE_SIZE,
        offset,
      },
    }),
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    count: deniesQuery.data?.count ?? 0,
    error: getErrorMessageIfAny(deniesQuery.error),
    isLoading: deniesQuery.isPending,
    rows: deniesQuery.data?.denies ?? [],
  };
}
