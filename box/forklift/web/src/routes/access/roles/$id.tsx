import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { RoleDetailPage } from "@/routes/access/roles/-components/role-detail-page";
import { canViewAccessManagement } from "@/utils/permissions";

export const Route = createFileRoute("/access/roles/$id")({
  component: RoleDetailRoute,
});

function RoleDetailRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAccessManagement(me)
    ? <RoleDetailPage me={me} />
    : <Redirect to="/workspace/repositories" replace />;
}
