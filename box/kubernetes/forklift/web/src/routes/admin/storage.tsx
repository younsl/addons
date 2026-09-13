import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { StoragePage } from "@/routes/admin/-storage/components/storage-page";
import { canViewAdministration } from "@/utils/permissions";

export const Route = createFileRoute("/admin/storage")({
  component: AdminStorageRoute,
});

function AdminStorageRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAdministration(me)
    ? <StoragePage />
    : <Redirect to="/workspace/repositories" replace />;
}
