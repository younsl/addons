import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { RolesPage } from "@/routes/access/roles/-components/roles-page";
import { canViewAccessManagement } from "@/utils/permissions";

export const Route = createFileRoute("/access/roles/")({
  component: RolesRoute,
});

function RolesRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAccessManagement(me)
    ? <RolesPage me={me} />
    : <Redirect to="/workspace/repositories" replace />;
}
