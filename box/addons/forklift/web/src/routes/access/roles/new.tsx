import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { RoleNewPage } from "@/routes/access/roles/-components/role-new-page";

export const Route = createFileRoute("/access/roles/new")({
  component: RoleNewRoute,
});

// Creating a role is admin-only, unlike viewing one - an auditor reaches
// /access/roles but never this page.
function RoleNewRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return me.admin ? <RoleNewPage /> : <Redirect to="/workspace/repositories" replace />;
}
