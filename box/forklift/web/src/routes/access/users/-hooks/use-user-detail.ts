import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// useUserDetail gathers what the modify page needs. As with roles there is no
// per-user endpoint, so the user comes out of the list. The role list is needed
// separately because the page offers roles the user does *not* hold, which the
// user record alone cannot supply.
export function useUserDetail(userId: number) {
  const [actionError, setActionError] = useState("");
  const enabled = Number.isFinite(userId);

  const usersQuery = useQuery({
    ...openApiQueryOptions.listUsers(),
    enabled,
    // Live refresh: the failed-login quota moves when someone is trying the
    // account's password right now, so keep the page current while it is open.
    // Only the user record is polled - the role and token lists do not move on
    // their own.
    refetchInterval: 5000,
    meta: { suppressGlobalErrorToast: true },
  });
  const rolesQuery = useQuery({
    ...openApiQueryOptions.listRoles(),
    enabled,
    meta: { suppressGlobalErrorToast: true },
  });
  const tokensQuery = useQuery({
    ...openApiQueryOptions.listUserTokens({ path: { id: userId } }),
    enabled,
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    // An action failure leads, being the most recent thing the user did.
    error:
      actionError ||
      getErrorMessageIfAny(usersQuery.error) ||
      getErrorMessageIfAny(rolesQuery.error) ||
      getErrorMessageIfAny(tokensQuery.error),
    isLoading: usersQuery.isPending || rolesQuery.isPending || tokensQuery.isPending,
    roles: rolesQuery.data ?? [],
    tokens: tokensQuery.data ?? [],
    user: usersQuery.data?.find((candidate) => candidate.id === userId),
    // The mutations invalidate what they change, so there is nothing to reload.
    runAction: (action: Promise<unknown>) => {
      setActionError("");
      action.catch((caught) => setActionError(getErrorMessage(caught)));
    },
    setError: setActionError,
  };
}
