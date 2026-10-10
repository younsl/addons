import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { RepositoriesPage } from "@/routes/workspace/repositories/-components/repositories-page";

export const Route = createFileRoute("/workspace/repositories/")({
  component: RepositoriesRoute,
});

// No permission gate: the directory is readable by any authenticated user, and
// the admin-only controls inside it gate themselves.
function RepositoriesRoute() {
  const { me } = useAuth();

  return <RepositoriesPage me={me} />;
}
