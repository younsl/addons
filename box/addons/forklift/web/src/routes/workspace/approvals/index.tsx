import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";
import { ApprovalsPage } from "@/routes/workspace/approvals/-components/approvals-page";
import { canViewApprovalQueue } from "@/utils/permissions";

type ApprovalsSearch = { repo?: string; status?: string; q?: string; page?: number };

// The queue's filters, search and page live in the URL so a filtered queue can
// be handed to a reviewer as a link.
export const Route = createFileRoute("/workspace/approvals/")({
  validateSearch: (search: Record<string, unknown>): ApprovalsSearch => {
    const out: ApprovalsSearch = {};
    for (const key of ["repo", "status", "q"] as const) {
      const value = search[key];
      if (typeof value === "string" && value !== "") out[key] = value;
    }
    const page = Number(search.page);
    if (Number.isInteger(page) && page > 1) out.page = page;
    return out;
  },
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
