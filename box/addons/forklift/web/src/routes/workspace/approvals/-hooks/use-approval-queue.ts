import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// The API pages the queue; 50 rows is what one screen of review comfortably
// holds without the pager becoming the main interaction.
export const APPROVALS_PAGE_SIZE = 50;

export const APPROVAL_STATUSES = ["pending", "approved", "rejected"] as const;

export type ApprovalStatus = (typeof APPROVAL_STATUSES)[number];

// useApprovalQueue fetches one page of the queue plus the pending count for the
// same repository scope. The count is a separate query because it is not the
// length of the page: it counts everything pending in scope, which is what the
// bulk-approve button needs to know.
export function useApprovalQueue({
  repo,
  status,
  page,
  q,
  regex,
}: {
  repo: string;
  status: string;
  page: number;
  q: string;
  regex: boolean;
}) {
  const approvalsQuery = useQuery({
    ...openApiQueryOptions.listApprovals({
      query: {
        repo: repo || undefined,
        status: (status || undefined) as ApprovalStatus | undefined,
        q: q || undefined,
        regex: regex || undefined,
        limit: APPROVALS_PAGE_SIZE,
        offset: page * APPROVALS_PAGE_SIZE,
      },
    }),
    meta: { suppressGlobalErrorToast: true },
  });

  const pendingCountQuery = useQuery({
    ...openApiQueryOptions.getApprovalsCount({
      query: { repo: repo || undefined, status: "pending" },
    }),
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    count: approvalsQuery.data?.count ?? 0,
    error: getErrorMessageIfAny(approvalsQuery.error),
    isLoading: approvalsQuery.isPending,
    // A failed count must not disable the bulk button by reading as zero, but
    // there is no better answer than zero either - so the count query's failure
    // is deliberately not surfaced. The queue's own error is the one that
    // matters, and it is shown.
    pendingCount: pendingCountQuery.data?.count ?? 0,
    rows: approvalsQuery.data?.approvals ?? [],
  };
}

// useUserIdsByUsername powers the requester links. Listing users is admin-only,
// so a non-admin approver gets an empty map and plain text instead - which is
// correct, since the user detail page is admin-only too.
export function useUserIdsByUsername() {
  const usersQuery = useQuery({
    ...openApiQueryOptions.listUsers(),
    meta: { suppressGlobalErrorToast: true },
  });

  return Object.fromEntries(
    (usersQuery.data ?? []).map((user) => [user.username, user.id]),
  );
}
