import { useMutation, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import {
  createRepositories,
  deleteRepository,
  postDisabledRepository,
  updateRepository,
  updateRepositorySecurity,
} from "@/services/v1/repositories/api";

import type {
  RepositoryCreate,
  RepositoryInput,
  RepositorySecurityInput,
} from "@/services/v1/openapi-types";

// Any change to a repository disturbs both the directory and that repository's
// own detail, and the name list that feeds every pattern combobox. Written by
// hand: the OpenAPI document does not say which reads a write invalidates.
function useInvalidateRepository() {
  const queryClient = useQueryClient();

  return async (repositoryId?: number) => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositories() }),
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositoryNames() }),
      repositoryId === undefined
        ? Promise.resolve()
        : queryClient.invalidateQueries({
            queryKey: openApiQueryKeys.getRepository({ path: { id: repositoryId } }),
          }),
    ]);
  };
}

export function useCreateRepositoryMutation() {
  const invalidateRepository = useInvalidateRepository();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: RepositoryCreate) => createRepositories({ body }),
    onSuccess: () => invalidateRepository(),
  });
}

export function useUpdateRepositoryMutation() {
  const invalidateRepository = useInvalidateRepository();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ repositoryId, body }: { repositoryId: number; body: RepositoryInput }) =>
      updateRepository({ path: { id: repositoryId }, body }),
    onSuccess: (_result, { repositoryId }) => invalidateRepository(repositoryId),
  });
}

// A separate endpoint from the general update: the security policy is a
// distinct grant (canEditRepositorySecurity), so it is a distinct write.
export function useUpdateRepositorySecurityMutation() {
  const queryClient = useQueryClient();
  const invalidateRepository = useInvalidateRepository();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({
      repositoryId,
      body,
    }: {
      repositoryId: number;
      body: RepositorySecurityInput;
    }) => updateRepositorySecurity({ path: { id: repositoryId }, body }),
    onSuccess: async (_result, { repositoryId }) => {
      await invalidateRepository(repositoryId);
      await Promise.all([
        // Turning approval on or off changes what the approval queue contains.
        queryClient.invalidateQueries({
          queryKey: operationKeyPrefix(openApiQueryKeys.listApprovals()),
        }),
        // This body carries notify.receivers, and a receiver's own record lists
        // the repositories pointing at it - which is what gates its deletion.
        queryClient.invalidateQueries({
          queryKey: openApiQueryKeys.listNotificationReceivers(),
        }),
      ]);
    },
  });
}

export function useSetRepositoryDisabledMutation() {
  const invalidateRepository = useInvalidateRepository();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ repositoryId, disabled }: { repositoryId: number; disabled: boolean }) =>
      postDisabledRepository({ path: { id: repositoryId }, body: { disabled } }),
    onSuccess: (_result, { repositoryId }) => invalidateRepository(repositoryId),
  });
}

export function useDeleteRepositoryMutation() {
  const invalidateRepository = useInvalidateRepository();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (repositoryId: number) => deleteRepository({ path: { id: repositoryId } }),
    // No per-repository invalidation: the repository is gone, so refetching its
    // detail would only produce a 404.
    onSuccess: () => invalidateRepository(),
  });
}
