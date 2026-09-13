import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import * as notificationApi from "@/services/v1/notification/api";
import { useReceiverForm } from "@/routes/admin/notifications/-hooks/use-receiver-form";
import {
  useDeleteReceiverMutation,
  useReceiversList,
} from "@/routes/admin/notifications/-hooks/use-receiver-mutations";

vi.mock("@/services/v1/notification/api");

// The hook navigates away on a successful save. There is no router here, so
// useNavigate is stubbed - what is under test is which endpoint the save hits,
// not where it lands afterwards.
const navigate = vi.fn();
vi.mock("@tanstack/react-router", () => ({ useNavigate: () => navigate }));

const mockedNotification = vi.mocked(notificationApi);

const receiver = {
  id: 2,
  name: "slack-security",
  description: "security channel",
  webhook_configured: true,
  enabled: true,
  created_by: "admin",
  created_at: "2026-01-01T00:00:00Z",
  repositories: ["npm-proxy"],
};

function withQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

  return {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    ),
  };
}

beforeEach(() => {
  mockedNotification.listNotificationReceivers.mockResolvedValue([receiver]);
  mockedNotification.createNotificationReceivers.mockResolvedValue(receiver);
  mockedNotification.updateNotificationReceivers.mockResolvedValue(receiver);
  mockedNotification.deleteNotificationReceivers.mockResolvedValue(undefined);
  mockedNotification.postTestNotification.mockResolvedValue({ status: "sent" });
  mockedNotification.postTestNotificationReceivers.mockResolvedValue({ status: "sent" });
});

describe("useReceiverForm", () => {
  test("an edit seeds the fields from the list, since there is no per-receiver endpoint", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    expect(result.current.form.description).toBe("security channel");
    expect(result.current.form.enabled).toBe(true);
  });

  // The webhook URL is write-only. Seeding it from the receiver is impossible -
  // the API never returns it - and pre-filling anything would be a lie about
  // what is stored.
  test("the webhook field starts blank even on an edit", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    expect(result.current.form.webhook_url).toBe("");
  });

  // A background refetch re-runs the query, and re-seeding on its result would
  // wipe whatever the admin had typed since.
  test("a refetch does not overwrite what the admin typed", async () => {
    const { wrapper } = withQueryClient();
    const { result, rerender } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    act(() => result.current.setForm({ ...result.current.form, name: "renamed" }));
    rerender();

    expect(result.current.form.name).toBe("renamed");
  });

  test("a create starts empty and does not wait on the list", () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(), { wrapper });

    expect(result.current.form.name).toBe("");
    expect(result.current.isLoading).toBe(false);
    expect(result.current.isEditing).toBe(false);
  });

  test("linked repositories come from the receiver, gating the delete", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.linkedRepositories).toEqual(["npm-proxy"]));
  });
});

// Which endpoint a test send hits depends on what is in the field: a typed URL
// is tested ad hoc so it can be verified before saving, and a blank field on an
// edit tests the stored URL the form cannot see.
describe("sending a test", () => {
  test("a typed URL is tested directly, not the stored one", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    act(() =>
      result.current.setForm({ ...result.current.form, webhook_url: "https://example.test/hook" }),
    );
    await act(async () => { result.current.sendTest(); });

    expect(mockedNotification.postTestNotification).toHaveBeenCalledWith({
      body: { webhook_url: "https://example.test/hook", name: "slack-security" },
    });
    expect(mockedNotification.postTestNotificationReceivers).not.toHaveBeenCalled();
  });

  test("a blank field on an edit tests the stored URL", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    await act(async () => { result.current.sendTest(); });

    expect(mockedNotification.postTestNotificationReceivers).toHaveBeenCalledWith({
      path: { id: 2 },
    });
    expect(mockedNotification.postTestNotification).not.toHaveBeenCalled();
  });

  test("a blank field on a create has nothing to test and says so", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(), { wrapper });

    await act(async () => { result.current.sendTest(); });

    expect(result.current.testError).not.toBe("");
    expect(mockedNotification.postTestNotification).not.toHaveBeenCalled();
    expect(mockedNotification.postTestNotificationReceivers).not.toHaveBeenCalled();
  });

  // A test send posts a sample payload and changes nothing on the server. The
  // missing invalidation is the point, not an omission.
  test("a test send does not refetch the receiver list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useReceiversList(), { wrapper });
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() =>
      expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(1),
    );
    await act(async () => { result.current.sendTest(); });

    expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(1);
  });
});

describe("saving", () => {
  test("an edit updates by id", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(2), { wrapper });

    await waitFor(() => expect(result.current.form.name).toBe("slack-security"));
    await act(async () => { result.current.save(); });

    expect(mockedNotification.updateNotificationReceivers).toHaveBeenCalledWith({
      path: { id: 2 },
      body: expect.objectContaining({ name: "slack-security", webhook_url: "" }),
    });
    expect(mockedNotification.createNotificationReceivers).not.toHaveBeenCalled();
  });

  test("a create posts a new receiver", async () => {
    const { wrapper } = withQueryClient();
    const { result } = renderHook(() => useReceiverForm(), { wrapper });

    act(() => result.current.setForm({ ...result.current.form, name: "pagerduty" }));
    await act(async () => { result.current.save(); });

    expect(mockedNotification.createNotificationReceivers).toHaveBeenCalledWith({
      body: expect.objectContaining({ name: "pagerduty" }),
    });
    expect(mockedNotification.updateNotificationReceivers).not.toHaveBeenCalled();
  });

  test("deleting refetches the list", async () => {
    const { wrapper } = withQueryClient();
    renderHook(() => useReceiversList(), { wrapper });
    const mutation = renderHook(() => useDeleteReceiverMutation(), { wrapper });

    await waitFor(() =>
      expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(1),
    );
    await act(async () => { await mutation.result.current.mutateAsync(2); });

    expect(mockedNotification.deleteNotificationReceivers).toHaveBeenCalledWith({ path: { id: 2 } });
    await waitFor(() =>
      expect(mockedNotification.listNotificationReceivers).toHaveBeenCalledTimes(2),
    );
  });
});
