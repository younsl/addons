import { createFileRoute } from "@tanstack/react-router";

import { Redirect } from "@/components/app/redirect";

export const Route = createFileRoute("/admin/")({
  // Admin surfaces now live as first-class sidebar destinations.
  component: () => <Redirect to="/admin/ha" replace />,
});
