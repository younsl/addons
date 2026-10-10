import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ReceiverFormPage } from "@/routes/admin/notifications/-components/receiver-form-page";
import { canViewAdministration } from "@/utils/permissions";

export const Route = createFileRoute("/admin/notifications/new")({
  component: AdminReceiverNewRoute,
});

function AdminReceiverNewRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAdministration(me)
    ? <ReceiverFormPage />
    : <Redirect to="/workspace/repositories" replace />;
}
