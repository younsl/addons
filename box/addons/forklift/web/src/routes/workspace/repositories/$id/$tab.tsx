import { createFileRoute } from "@tanstack/react-router";
import { useAuth } from "@/authContext";
import { RepositoryDetail } from "@/routes/workspace/repositories/-components/detail/repository-detail-page";
import { type ArtifactFilter, parseArtifactFilter } from "@/routes/workspace/repositories/-utils/artifact-filter";

// `filter` carries a Statistics panel's drill-down into the Artifacts tab, so
// the filtered view is a link that can be shared and survives a reload.
export const Route = createFileRoute("/workspace/repositories/$id/$tab")({
  validateSearch: (search: Record<string, unknown>): { filter?: ArtifactFilter } => {
    const filter = parseArtifactFilter(search.filter);
    return filter ? { filter } : {};
  },
  component: RepositoryDetailRoute,
});

function RepositoryDetailRoute() {
  const { me } = useAuth();
  return <RepositoryDetail me={me} />;
}
