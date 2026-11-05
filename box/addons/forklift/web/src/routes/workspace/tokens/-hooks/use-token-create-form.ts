import { useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";

import { getErrorMessage } from "@/lib/http/error/api-error";
import { useCreateTokenMutation } from "@/routes/workspace/tokens/-hooks/use-token-mutations";
import {
  startOfLocalDay,
  toExpiresIn,
} from "@/routes/workspace/tokens/-utils/token-expiry";
import { useTranslation } from "@/lib/i18n";

import type { TokenScope } from "@/services/v1/openapi-types";

// useTokenCreateForm holds the whole create page: which identity the token is
// for, the fields, and the secret that comes back once. The presence of an :id
// route param is what selects the target - the same page serves self-service
// from /workspace/tokens/new and an admin issuing for someone else from
// /access/users/$id/tokens/new.
export function useTokenCreateForm() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams({ strict: false }) as { id?: string };
  const forUserId = id ? Number(id) : null;

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [scopes, setScopes] = useState<TokenScope[]>([]);
  const [expiresOn, setExpiresOn] = useState<Date | undefined>();
  const [error, setError] = useState("");
  // The secret is shown once and never fetched again, so it lives in state
  // rather than in the cache: a refetch could not produce it a second time.
  const [createdToken, setCreatedToken] = useState("");
  const createTokenMutation = useCreateTokenMutation();

  const today = startOfLocalDay(new Date());
  // At least tomorrow: a token expiring today is spent before it is used.
  const minDate = new Date(today);
  minDate.setDate(today.getDate() + 1);
  // The API caps a token at a year, so the picker does too - a date it would
  // reject should not be selectable.
  const maxDate = new Date(today);
  maxDate.setDate(today.getDate() + 365);

  const isComplete = Boolean(
    name.trim() && description.trim() && scopes.length > 0 && expiresOn,
  );

  return {
    createdToken,
    description,
    error,
    expiresOn,
    forUserId,
    isComplete,
    isPending: createTokenMutation.isPending,
    maxDate,
    minDate,
    name,
    scopes,
    setDescription,
    setExpiresOn,
    setName,
    setScopes,
    // Where Done and Cancel return to: back where the page was entered from.
    returnTo: () =>
      forUserId !== null
        ? navigate({ to: "/access/users/$id", params: { id: String(forUserId) } })
        : navigate({ to: "/workspace/tokens" }),
    submit: () => {
      setError("");
      if (!isComplete || !expiresOn) {
        setError(t("token.incomplete-note"));
        return;
      }
      createTokenMutation.mutate(
        {
          forUserId,
          body: {
            name: name.trim(),
            description: description.trim(),
            scopes,
            expires_in: toExpiresIn(expiresOn, startOfLocalDay),
          },
        },
        {
          onSuccess: (issued) => setCreatedToken(issued.token),
          onError: (caught) => setError(getErrorMessage(caught)),
        },
      );
    },
  };
}
