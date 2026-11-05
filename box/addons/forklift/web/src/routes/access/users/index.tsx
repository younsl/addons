import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { UsersPage } from "@/routes/access/users/-components/users-page";
import { canViewAccessManagement } from "@/utils/permissions";

export const Route = createFileRoute("/access/users/")({
  component: UsersRoute,
});

function UsersRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewAccessManagement(me)
    ? <UsersPage me={me} />
    : <Redirect to="/workspace/repositories" replace />;
}
