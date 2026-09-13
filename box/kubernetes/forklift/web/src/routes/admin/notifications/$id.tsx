import { createFileRoute, useParams } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ReceiverFormPage } from "@/routes/admin/notifications/-components/receiver-form-page";
import { canViewAdministration } from "@/utils/permissions";

export const Route = createFileRoute("/admin/notifications/$id")({
  component: AdminReceiverEditRoute,
});

// Same form as /new; passing a receiverId is what turns it into an edit.
function AdminReceiverEditRoute() {
  const { me } = useAuth();
  const { id } = useParams({ from: "/admin/notifications/$id" });

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAdministration(me)
    ? <ReceiverFormPage receiverId={Number(id)} />
    : <Redirect to="/workspace/repositories" replace />;
}
