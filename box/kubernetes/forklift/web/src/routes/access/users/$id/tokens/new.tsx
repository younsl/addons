import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { TokenNewPage } from "@/routes/workspace/tokens/-components/token-new-page";

export const Route = createFileRoute("/access/users/$id/tokens/new")({
  component: UserTokenNewRoute,
});

// Same page as the self-service one; the :id route param is what makes it issue
// a token for that user instead of for the admin. Issuing on someone else's
// behalf is admin-only.
function UserTokenNewRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return me.admin ? <TokenNewPage /> : <Redirect to="/workspace/repositories" replace />;
}
