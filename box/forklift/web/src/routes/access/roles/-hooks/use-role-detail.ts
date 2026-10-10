import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// useRoleDetail resolves one role and the users holding it. There is no
// per-role endpoint: the role comes from the role list, and its membership from
// the user list, because a user carries its roles rather than a role carrying
// its users. That is not visible from the calls alone, hence this note.
export function useRoleDetail(roleId: number) {
  const [actionError, setActionError] = useState("");
  // A non-numeric :id in the URL would otherwise fetch both lists only to match
  // nothing; skip the requests and let the screen report the miss.
  const enabled = Number.isFinite(roleId);

  const rolesQuery = useQuery({
    ...openApiQueryOptions.listRoles(),
    enabled,
    meta: { suppressGlobalErrorToast: true },
  });
  const usersQuery = useQuery({
    ...openApiQueryOptions.listUsers(),
    enabled,
    meta: { suppressGlobalErrorToast: true },
  });

  const role = rolesQuery.data?.find((candidate) => candidate.id === roleId);
  const members = (usersQuery.data ?? []).filter((user) =>
    user.roles.some((userRole) => userRole.id === roleId),
  );

  return {
    // One string, not two query errors plus a mutation error: the screen has a
    // single alert slot, so the combining happens once here instead of being
    // repeated at every call site. An action failure leads, being the most
    // recent thing the user did.
    error:
      actionError ||
      getErrorMessageIfAny(rolesQuery.error) ||
      getErrorMessageIfAny(usersQuery.error),
    isLoading: rolesQuery.isPending || usersQuery.isPending,
    members,
    role,
    // Awaits a mutation and turns its rejection into the screen's error. The
    // mutations invalidate what they change, so there is nothing to reload here.
    runAction: (action: Promise<unknown>) => {
      setActionError("");
      action.catch((caught) => setActionError(getErrorMessage(caught)));
    },
    setError: setActionError,
  };
}
