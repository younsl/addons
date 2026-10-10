import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { useApproveAllPendingMutation } from "@/routes/workspace/approvals/-hooks/use-approval-mutations";

// useBulkApproval drives the one-row-per-repository bulk screen: which
// repositories have a pending queue, the shared decision (everything, or only
// the packages with no known advisories), and the approve that acts on one row.
export function useBulkApproval() {
  const [cleanOnly, setCleanOnly] = useState(false);
  const [note, setNote] = useState("");
  const [actionError, setActionError] = useState("");
  const [doneMessage, setDoneMessage] = useState("");
  // Which row is in flight, so only that row's button shows as busy. A single
  // boolean would grey out every button and hide which one was pressed.
  const [busyRepo, setBusyRepo] = useState<string | null>(null);
  const approveAllMutation = useApproveAllPendingMutation();

  const pendingReposQuery = useQuery({
    ...openApiQueryOptions.listApprovalsPendingRepos(),
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    busyRepo,
    cleanOnly,
    doneMessage,
    error: actionError || getErrorMessageIfAny(pendingReposQuery.error),
    isLoading: pendingReposQuery.isPending,
    note,
    repos: pendingReposQuery.data?.repos ?? [],
    setCleanOnly,
    setNote,
    approve: (repoName: string) => {
      setBusyRepo(repoName);
      setActionError("");
      setDoneMessage("");
      approveAllMutation.mutate(
        { repo: repoName, note, cleanOnly },
        {
          onSuccess: (result) =>
            setDoneMessage(
              `Approved ${result.approved} ${result.approved === 1 ? "package" : "packages"} in ${repoName}.`,
            ),
          onError: (caught) => setActionError(getErrorMessage(caught)),
          onSettled: () => setBusyRepo(null),
        },
      );
    },
  };
}
