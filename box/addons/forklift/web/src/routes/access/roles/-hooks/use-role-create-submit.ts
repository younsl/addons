import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { getErrorMessage } from "@/lib/http/error/api-error";
import { useCreateRoleMutation } from "@/routes/access/roles/-hooks/use-role-mutations";

import type { RoleCreate } from "@/services/v1/openapi-types";

// useRoleCreateSubmit owns what happens after the create form is filled in:
// the call, the error it may raise, and the return to the directory.
export function useRoleCreateSubmit() {
  const navigate = useNavigate();
  const [error, setError] = useState("");
  const createRoleMutation = useCreateRoleMutation();

  return {
    error,
    isPending: createRoleMutation.isPending,
    submit: (body: RoleCreate) => {
      setError("");
      createRoleMutation.mutate(body, {
        // Navigating on success, not on settle: a failed create has to leave the
        // form standing with its values, or the user retypes everything.
        onSuccess: () => navigate({ to: "/access/roles" }),
        onError: (caught) => setError(getErrorMessage(caught)),
      });
    },
  };
}
