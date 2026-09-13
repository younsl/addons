import { createFileRoute } from "@tanstack/react-router";

import { Redirect } from "@/components/app/redirect";

export const Route = createFileRoute("/workspace/")({
  component: () => <Redirect to="/workspace/repositories" replace />,
});
