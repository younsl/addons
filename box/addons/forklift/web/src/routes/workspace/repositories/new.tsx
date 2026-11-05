import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { RepositoryNewPage } from "@/routes/workspace/repositories/-components/repository-new-page";

export const Route = createFileRoute("/workspace/repositories/new")({
  component: RepositoryNewRoute,
});

// Browsing repositories is open to any authenticated user; creating one is not.
function RepositoryNewRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return me.admin ? <RepositoryNewPage /> : <Redirect to="/workspace/repositories" replace />;
}
