import { useMutation, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import {
  createRolePermissions,
  createRoles,
  deleteRole,
  deleteRolePermissions,
} from "@/services/v1/roles/api";

import type { PermissionCreate, RoleCreate } from "@/services/v1/openapi-types";

// The invalidation rules live here rather than in the generator. That a role
// permission change has to refresh the role list is not something the OpenAPI
// document states - letting codegen guess it would mean inventing facts the
// document does not carry. Keeping the rules by hand also keeps them reviewable.
function useInvalidateRoles() {
  const queryClient = useQueryClient();

  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRoles() }),
      // A role grants access by repository pattern, so a permission change can
      // alter who may reach any repository - and the repository detail's
      // Permissions tab lists exactly that. Which repositories a pattern covers
      // is not knowable here, so every one of them is invalidated.
      queryClient.invalidateQueries({
        queryKey: operationKeyPrefix(
          openApiQueryKeys.listRepositoryPermissions({ path: { id: 0 } }),
        ),
      }),
    ]);
  };
}

export function useCreateRoleMutation() {
  const invalidateRoles = useInvalidateRoles();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: RoleCreate) => createRoles({ body }),
    onSuccess: invalidateRoles,
  });
}

export function useDeleteRoleMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    // The screen passes an id; the OpenAPI parameter shape stays in here.
    mutationFn: (roleId: number) => deleteRole({ path: { id: roleId } }),
    onSuccess: async () => {
      // Deleting a role also changes every user that held it, so the user list
      // is invalidated too - a role a user no longer has must stop being drawn.
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRoles() }),
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listUsers() }),
      ]);
    },
  });
}

export function useAddRolePermissionMutation() {
  const invalidateRoles = useInvalidateRoles();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ roleId, permission }: { roleId: number; permission: PermissionCreate }) =>
      createRolePermissions({ path: { id: roleId }, body: permission }),
    onSuccess: invalidateRoles,
  });
}

export function useRemoveRolePermissionMutation() {
  const invalidateRoles = useInvalidateRoles();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ roleId, permissionId }: { roleId: number; permissionId: number }) =>
      deleteRolePermissions({ path: { id: roleId, permID: permissionId } }),
    onSuccess: invalidateRoles,
  });
}
