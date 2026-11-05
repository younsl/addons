import { useMutation, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import {
  createUserRoles,
  createUsers,
  deleteUser,
  deleteUserRoles,
  postImpersonateUser,
  updateUser,
} from "@/services/v1/users/api";
import {
  deleteUserTokens,
  updateUserTokens,
} from "@/services/v1/tokens/api";

import type { TokenScope, UserCreate, UserInput } from "@/services/v1/openapi-types";

// The invalidation rules are written by hand, not generated: the OpenAPI
// document says nothing about which reads a write disturbs.
function useInvalidateUsers() {
  const queryClient = useQueryClient();

  return () =>
    queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() });
}

function useInvalidateUserTokens() {
  const queryClient = useQueryClient();

  return (userId: number) =>
    Promise.all([
      queryClient.invalidateQueries({
        queryKey: openApiQueryKeys.listUserTokens({ path: { id: userId } }),
      }),
      // The directory shows a token count per user, so it goes stale too.
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() }),
      // And the repository detail's Permissions tab lists the tokens scoped to
      // it; which repositories a scope covers is not knowable here.
      queryClient.invalidateQueries({
        queryKey: operationKeyPrefix(
          openApiQueryKeys.listRepositoryTokens({ path: { id: 0 } }),
        ),
      }),
    ]);
}

export function useCreateUserMutation() {
  const invalidateUsers = useInvalidateUsers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: UserCreate) => createUsers({ body }),
    onSuccess: invalidateUsers,
  });
}

// One mutation covers password reset, enable/disable, lockout and unlock: they
// are all PUT /users/{id} with a different field set, and the screen shows them
// as separate controls only because they are separate decisions.
export function useUpdateUserMutation() {
  const invalidateUsers = useInvalidateUsers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ userId, body }: { userId: number; body: UserInput }) =>
      updateUser({ path: { id: userId }, body }),
    onSuccess: invalidateUsers,
  });
}

export function useDeleteUserMutation() {
  const invalidateUsers = useInvalidateUsers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (userId: number) => deleteUser({ path: { id: userId } }),
    onSuccess: invalidateUsers,
  });
}

export function useAssignUserRoleMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ userId, roleId }: { userId: number; roleId: number }) =>
      createUserRoles({ path: { id: userId }, body: { role_id: roleId } }),
    onSuccess: async () => {
      // The role list carries a user_count, so assigning changes both sides.
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() }),
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRoles() }),
      ]);
    },
  });
}

export function useRemoveUserRoleMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ userId, roleId }: { userId: number; roleId: number }) =>
      deleteUserRoles({ path: { id: userId, roleID: roleId } }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() }),
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRoles() }),
      ]);
    },
  });
}

// No invalidation: the response replaces the session cookie, so the caller
// reloads the app rather than refreshing a query. Every cached entry belongs to
// the previous identity and has to be dropped wholesale.
export function useImpersonateUserMutation() {
  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ userId, reason }: { userId: number; reason: string }) =>
      postImpersonateUser({ path: { id: userId }, body: { reason } }),
  });
}

export function useUpdateUserTokenScopesMutation() {
  const invalidateUserTokens = useInvalidateUserTokens();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({
      userId,
      tokenId,
      scopes,
    }: {
      userId: number;
      tokenId: number;
      scopes: TokenScope[];
    }) => updateUserTokens({ path: { id: userId, tokenID: tokenId }, body: { scopes } }),
    onSuccess: (_result, { userId }) => invalidateUserTokens(userId),
  });
}

export function useRevokeUserTokenMutation() {
  const invalidateUserTokens = useInvalidateUserTokens();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ userId, tokenId }: { userId: number; tokenId: number }) =>
      deleteUserTokens({ path: { id: userId, tokenID: tokenId } }),
    onSuccess: (_result, { userId }) => invalidateUserTokens(userId),
  });
}
