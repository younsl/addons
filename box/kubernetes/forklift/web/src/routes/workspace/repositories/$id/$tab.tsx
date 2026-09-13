import { createFileRoute } from "@tanstack/react-router";
import { useAuth } from "@/authContext";
import { RepositoryDetail } from "@/routes/workspace/repositories/-components/detail/repository-detail-page";

export const Route = createFileRoute("/workspace/repositories/$id/$tab")({
  component: RepositoryDetailRoute,
});

function RepositoryDetailRoute() {
  const { me } = useAuth();
  return <RepositoryDetail me={me} />;
}
