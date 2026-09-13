import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// Unlike roles and users, an approval does have its own endpoint - the detail
// page carries the full OSV analysis, which the list rows do not.
export function useApprovalDetail(approvalId: number) {
  const approvalQuery = useQuery({
    ...openApiQueryOptions.getApproval({ path: { id: approvalId } }),
    enabled: Number.isFinite(approvalId),
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    approval: approvalQuery.data,
    error: getErrorMessageIfAny(approvalQuery.error),
    isLoading: approvalQuery.isPending,
  };
}
