import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ReceiversPage } from "@/routes/admin/notifications/-components/receivers-page";
import { canViewAdministration } from "@/utils/permissions";

export const Route = createFileRoute("/admin/notifications/")({
  component: AdminNotificationsRoute,
});

function AdminNotificationsRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAdministration(me)
    ? <ReceiversPage />
    : <Redirect to="/workspace/repositories" replace />;
}
