import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ApprovalDetailPage } from "@/routes/workspace/approvals/-components/approval-detail-page";
import { canViewApprovalQueue } from "@/utils/permissions";

export const Route = createFileRoute("/workspace/approvals/$id")({
  component: ApprovalDetailRoute,
});

function ApprovalDetailRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewApprovalQueue(me)
    ? <ApprovalDetailPage />
    : <Redirect to="/workspace/repositories" replace />;
}
