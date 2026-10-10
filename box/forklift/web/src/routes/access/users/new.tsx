import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { UserNewPage } from "@/routes/access/users/-components/user-new-page";

export const Route = createFileRoute("/access/users/new")({
  component: UserNewRoute,
});

// Creating a user is admin-only, unlike viewing one - an auditor reaches
// /access/users but never this page.
function UserNewRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return me.admin ? <UserNewPage /> : <Redirect to="/workspace/repositories" replace />;
}
