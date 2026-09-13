import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ApprovalsPage } from "@/routes/workspace/approvals/-components/approvals-page";
import { canViewApprovalQueue } from "@/utils/permissions";

export const Route = createFileRoute("/workspace/approvals/")({
  component: ApprovalsRoute,
});

function ApprovalsRoute() {
  const { me } = useAuth();

  // Signed out is not "not allowed": the shell is already redirecting to
  // /login, and a second redirect from here would fight it.
  if (!me.authenticated) return null;

  return canViewApprovalQueue(me)
    ? <ApprovalsPage />
    : <Redirect to="/workspace/repositories" replace />;
}
