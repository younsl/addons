import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import {
  createNotificationReceivers,
  deleteNotificationReceivers,
  postTestNotification,
  postTestNotificationReceivers,
  updateNotificationReceivers,
} from "@/services/v1/notification/api";

import type { ReceiverInput } from "@/services/v1/openapi-types";

export function useReceiversList() {
  return useQuery({
    ...openApiQueryOptions.listNotificationReceivers(),
    meta: { suppressGlobalErrorToast: true },
  });
}

function useInvalidateReceivers() {
  const queryClient = useQueryClient();

  return () =>
    queryClient.invalidateQueries({
      queryKey: openApiQueryKeys.listNotificationReceivers(),
    });
}

export function useCreateReceiverMutation() {
  const invalidateReceivers = useInvalidateReceivers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (body: ReceiverInput) => createNotificationReceivers({ body }),
    onSuccess: invalidateReceivers,
  });
}

export function useUpdateReceiverMutation() {
  const invalidateReceivers = useInvalidateReceivers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ receiverId, body }: { receiverId: number; body: ReceiverInput }) =>
      updateNotificationReceivers({ path: { id: receiverId }, body }),
    onSuccess: invalidateReceivers,
  });
}

export function useDeleteReceiverMutation() {
  const invalidateReceivers = useInvalidateReceivers();

  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (receiverId: number) =>
      deleteNotificationReceivers({ path: { id: receiverId } }),
    onSuccess: invalidateReceivers,
  });
}

// The two test sends change nothing on the server - they post a sample payload
// to a webhook - so neither invalidates anything. The absence is deliberate,
// not an omission.
//
// Which one runs depends on what the operator has in the field. A typed URL is
// tested ad hoc, so it can be verified before being saved; a blank field on an
// edit tests the stored URL, which the API never returns and the form therefore
// cannot send back.
export function useTestWebhookUrlMutation() {
  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: ({ url, name }: { url: string; name: string }) =>
      postTestNotification({ body: { webhook_url: url, name } }),
  });
}

export function useTestStoredReceiverMutation() {
  return useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: (receiverId: number) =>
      postTestNotificationReceivers({ path: { id: receiverId } }),
  });
}
