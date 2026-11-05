import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import {
  createTokens,
  createUserTokens,
  deleteToken,
  updateToken,
} from "@/services/v1/tokens/api";

import type { TokenCreate, TokenScope } from "@/services/v1/openapi-types";

// useTokensList is the current user's own tokens - a different endpoint and a
// different key from a named user's tokens on the admin detail page.
export function useTokensList() {
  return useQuery({
    ...openApiQueryOptions.listTokens(),
    meta: { suppressGlobalErrorToast: true },
  });
}

// A token's scopes decide which repositories it may reach, and the repository
// detail's Permissions tab lists the tokens scoped to it. Which repositories a
// scope pattern covers is not knowable here, so every one of them goes.
function useInvalidateRepositoryTokens() {
  const queryClient = useQueryClient();

  return () =>
    queryClient.invalidateQueries({
      queryKey: operationKeyPrefix(
        openApiQueryKeys.listRepositoryTokens({ path: { id: 0 } }),
      ),
    });
}

function useInvalidateTokens() {
  const queryClient = useQueryClient();
  const invalidateRepositoryTokens = useInvalidateRepositoryTokens();

  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listTokens() }),
      invalidateRepositoryTokens(),
    ]);
  };
}

export function useRevokeTokenMutation() {
  const invalidateTokens = useInvalidateTokens();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (tokenId: number) => deleteToken({ path: { id: tokenId } }),
    onSuccess: invalidateTokens,
  });
}

export function useUpdateTokenScopesMutation() {
  const invalidateTokens = useInvalidateTokens();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ tokenId, scopes }: { tokenId: number; scopes: TokenScope[] }) =>
      updateToken({ path: { id: tokenId }, body: { scopes } }),
    onSuccess: invalidateTokens,
  });
}

// One page creates tokens two ways: for yourself, or - when an admin arrives
// from a user's detail page - for that user. Two endpoints, so which lists go
// stale differs, and the branch belongs here rather than in the form.
export function useCreateTokenMutation() {
  const queryClient = useQueryClient();
  const invalidateRepositoryTokens = useInvalidateRepositoryTokens();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ forUserId, body }: { forUserId: number | null; body: TokenCreate }) =>
      forUserId === null
        ? createTokens({ body })
        : createUserTokens({ path: { id: forUserId }, body }),
    onSuccess: async (_result, { forUserId }) => {
      // A new token can be scoped to any repository, so the Permissions tab of
      // every repository is stale either way.
      await invalidateRepositoryTokens();

      if (forUserId === null) {
        await queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listTokens() });
        return;
      }

      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: openApiQueryKeys.listUserTokens({ path: { id: forUserId } }),
        }),
        // The user directory shows a token count.
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() }),
      ]);
    },
  });
}
