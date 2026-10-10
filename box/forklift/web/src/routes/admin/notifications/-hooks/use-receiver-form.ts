import { useEffect, useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import {
  useCreateReceiverMutation,
  useDeleteReceiverMutation,
  useReceiversList,
  useTestStoredReceiverMutation,
  useTestWebhookUrlMutation,
  useUpdateReceiverMutation,
} from "@/routes/admin/notifications/-hooks/use-receiver-mutations";

import type { ReceiverInput } from "@/services/v1/openapi-types";

const EMPTY_FORM: ReceiverInput = {
  name: "",
  description: "",
  webhook_url: "",
  enabled: true,
};

// useReceiverForm drives the shared create/edit form. There is no per-receiver
// endpoint, so an edit finds its receiver in the list - the same shape as roles
// and users.
export function useReceiverForm(receiverId?: number) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const isEditing = receiverId !== undefined;
  const [form, setForm] = useState<ReceiverInput>(EMPTY_FORM);
  const [actionError, setActionError] = useState("");
  const [testMessage, setTestMessage] = useState("");
  const [testError, setTestError] = useState("");

  const receiversQuery = useReceiversList();
  const createMutation = useCreateReceiverMutation();
  const updateMutation = useUpdateReceiverMutation();
  const deleteMutation = useDeleteReceiverMutation();
  const testUrlMutation = useTestWebhookUrlMutation();
  const testStoredMutation = useTestStoredReceiverMutation();

  const receiver = isEditing
    ? receiversQuery.data?.find((candidate) => candidate.id === receiverId)
    : undefined;
  // Repositories whose notify config selects this receiver. Deletion stays
  // disabled while any remain, mirroring the API's 409 guard.
  const linkedRepositories = receiver?.repositories ?? [];

  // The form is seeded from the fetched receiver once and then owned by the
  // user. Re-seeding on every render of the query data would discard whatever
  // they had typed the moment a background refetch landed.
  //
  // webhook_url starts blank even on an edit: the API never returns it, so
  // there is nothing to seed. Leaving it blank on save keeps the stored URL;
  // a non-empty value replaces it.
  useEffect(() => {
    if (!receiver) return;
    setForm({
      name: receiver.name,
      description: receiver.description,
      webhook_url: "",
      enabled: receiver.enabled,
    });
  }, [receiver?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const save = () => {
    setActionError("");
    const handlers = {
      onSuccess: () => navigate({ to: "/admin/notifications" }),
      onError: (caught: unknown) => setActionError(getErrorMessage(caught)),
    };

    if (isEditing) updateMutation.mutate({ receiverId, body: form }, handlers);
    else createMutation.mutate(form, handlers);
  };

  return {
    error: actionError || getErrorMessageIfAny(receiversQuery.error),
    form,
    isEditing,
    // Only an edit has to wait: a create has nothing to load.
    isLoading: isEditing && receiversQuery.isPending,
    isSaving: createMutation.isPending || updateMutation.isPending,
    isTesting: testUrlMutation.isPending || testStoredMutation.isPending,
    linkedRepositories,
    testError,
    testMessage,
    setForm,
    cancel: () => navigate({ to: "/admin/notifications" }),
    remove: () => {
      if (receiverId === undefined) return;
      setActionError("");
      deleteMutation.mutate(receiverId, {
        onSuccess: () => navigate({ to: "/admin/notifications" }),
        onError: (caught) => setActionError(getErrorMessage(caught)),
      });
    },
    save,
    sendTest: () => {
      setTestMessage("");
      setTestError("");
      const url = form.webhook_url?.trim() ?? "";
      // Nothing typed and nothing stored: there is no webhook to test.
      if (!url && !isEditing) {
        setTestError(t("notification.webhook-required"));
        return;
      }

      const handlers = {
        onSuccess: () => setTestMessage(t("notification.test-sent")),
        onError: (caught: unknown) => setTestError(getErrorMessage(caught)),
      };

      if (url) testUrlMutation.mutate({ url, name: form.name }, handlers);
      else testStoredMutation.mutate(receiverId!, handlers);
    },
  };
}
