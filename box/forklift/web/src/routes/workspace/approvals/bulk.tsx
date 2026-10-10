import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { BulkApprovalPage } from "@/routes/workspace/approvals/-components/bulk-approval-page";
import { canViewApprovalQueue } from "@/utils/permissions";

export const Route = createFileRoute("/workspace/approvals/bulk")({
  component: BulkApprovalRoute,
});

// Admins, approvers and auditors may all reach the bulk screen; whether they
// can act on it is decided inside. Anyone else goes back to the queue, which
// blocks them in turn.
function BulkApprovalRoute() {
  const { me } = useAuth();

  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;

  return canViewApprovalQueue(me)
    ? <BulkApprovalPage />
    : <Redirect to="/workspace/approvals" replace />;
}
