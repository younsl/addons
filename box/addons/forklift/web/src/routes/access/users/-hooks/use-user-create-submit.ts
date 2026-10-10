import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";

import { getErrorMessage } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { useCreateUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { UserCreate } from "@/services/v1/openapi-types";

export function useUserCreateSubmit() {
  const navigate = useNavigate();
  const [error, setError] = useState("");
  const createUserMutation = useCreateUserMutation();
  // Offered as an optional initial role. A failed fetch leaves the picker empty
  // rather than blocking creation: a role can always be assigned afterwards.
  const rolesQuery = useQuery({
    ...openApiQueryOptions.listRoles(),
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    error,
    isPending: createUserMutation.isPending,
    roles: rolesQuery.data ?? [],
    setError,
    submit: (body: UserCreate) => {
      setError("");
      createUserMutation.mutate(body, {
        // Only on success: a rejected create has to leave the form standing,
        // and the password fields are not worth retyping.
        onSuccess: () => navigate({ to: "/access/users" }),
        onError: (caught) => setError(getErrorMessage(caught)),
      });
    },
  };
}
