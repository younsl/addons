import { createFileRoute } from "@tanstack/react-router";

import { Redirect } from "@/components/app/redirect";

export const Route = createFileRoute("/access/")({
  component: () => <Redirect to="/access/users" replace />,
});
