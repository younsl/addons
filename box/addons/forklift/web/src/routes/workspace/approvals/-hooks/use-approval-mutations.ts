import { useMutation, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import {
  createApprovals,
  createVersionDenies,
  deleteVersionDeny,
  postApproveAllApprovals,
  postApproveApproval,
  postRejectApproval,
} from "@/services/v1/approvals/api";

import type {
  PackageApprovalCreate,
  VersionDenyCreate,
} from "@/services/v1/openapi-types";

// A decision moves a request between the pending, approved and rejected views
// and changes the pending count in the sidebar, so all three query families go
// stale at once. The queue is keyed by its filters - repository, status, page,
// search - so invalidating one exact key would leave every other filter showing
// the row in its old state. operationKeyPrefix covers all of them.
//
// None of this is in the OpenAPI document; it is the sort of rule that has to
// be written by hand rather than guessed by a generator.
function useInvalidateApprovals() {
  const queryClient = useQueryClient();

  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: operationKeyPrefix(openApiQueryKeys.listApprovals()),
      }),
      queryClient.invalidateQueries({
        queryKey: operationKeyPrefix(openApiQueryKeys.getApprovalsCount()),
      }),
      queryClient.invalidateQueries({
        queryKey: openApiQueryKeys.listApprovalsPendingRepos(),
      }),
      // The repository directory carries pending_approval_count and draws it as
      // a badge, so a decision made here changes what that list says too.
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositories() }),
    ]);
  };
}

function useInvalidateVersionDenies() {
  const queryClient = useQueryClient();

  return () =>
    queryClient.invalidateQueries({
      queryKey: operationKeyPrefix(openApiQueryKeys.listVersionDenies()),
    });
}

export function useDecideApprovalMutation() {
  const queryClient = useQueryClient();
  const invalidateApprovals = useInvalidateApprovals();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    // Approve and reject are the same decision with opposite outcomes, made in
    // the same modal, so one mutation covers both rather than the modal picking
    // between two hooks.
    mutationFn: ({
      approvalId,
      decision,
      note,
    }: {
      approvalId: number;
      decision: "approve" | "reject";
      note: string;
    }) =>
      decision === "approve"
        ? postApproveApproval({ path: { id: approvalId }, body: { note } })
        : postRejectApproval({ path: { id: approvalId }, body: { note } }),
    onSuccess: async (_result, { approvalId }) => {
      await Promise.all([
        invalidateApprovals(),
        // The detail page, if it is what raised this.
        queryClient.invalidateQueries({
          queryKey: openApiQueryKeys.getApproval({ path: { id: approvalId } }),
        }),
      ]);
    },
  });
}

export function useApproveAllPendingMutation() {
  const invalidateApprovals = useInvalidateApprovals();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ repo, note, cleanOnly }: { repo: string; note: string; cleanOnly: boolean }) =>
      postApproveAllApprovals({ body: { repo, note, clean_only: cleanOnly } }),
    onSuccess: invalidateApprovals,
  });
}

// Recording a rule ahead of demand: an approval decision for a package that
// nobody has requested yet.
export function useCreateApprovalMutation() {
  const invalidateApprovals = useInvalidateApprovals();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: PackageApprovalCreate) => createApprovals({ body }),
    onSuccess: invalidateApprovals,
  });
}

export function useCreateVersionDenyMutation() {
  const invalidateVersionDenies = useInvalidateVersionDenies();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: VersionDenyCreate) => createVersionDenies({ body }),
    onSuccess: invalidateVersionDenies,
  });
}

export function useRemoveVersionDenyMutation() {
  const invalidateVersionDenies = useInvalidateVersionDenies();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (denyId: number) => deleteVersionDeny({ path: { id: denyId } }),
    onSuccess: invalidateVersionDenies,
  });
}
