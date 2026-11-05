import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { HaStatusPage } from "@/routes/admin/-ha/components/ha-status-page";
import { canViewAdministration } from "@/utils/permissions";

export const Route = createFileRoute("/admin/ha")({
  component: AdminHaRoute,
});

function AdminHaRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAdministration(me)
    ? <HaStatusPage />
    : <Redirect to="/workspace/repositories" replace />;
}
