import { createFileRoute } from "@tanstack/react-router";
import { useAuth } from "@/authContext";
import { RepositoryDetail } from "@/routes/workspace/repositories/-components/detail/repository-detail-page";
import { type ArtifactFilter, parseArtifactFilter } from "@/routes/workspace/repositories/-utils/artifact-filter";

// Everything that shapes a tab's view is kept in the URL so the view can be
// shared and survives a reload: `filter` (a Statistics panel's drill-down into
// the Artifacts tab), the table search `q` and `page`, the Audit tab's `event`
// filter and the Approvals tab's `status`.
type TabSearch = { filter?: ArtifactFilter; q?: string; page?: number; event?: string; status?: string };

export const Route = createFileRoute("/workspace/repositories/$id/$tab")({
  validateSearch: (search: Record<string, unknown>): TabSearch => {
    const out: TabSearch = {};
    const filter = parseArtifactFilter(search.filter);
    if (filter) out.filter = filter;
    if (typeof search.q === "string" && search.q !== "") out.q = search.q;
    const page = Number(search.page);
    if (Number.isInteger(page) && page > 1) out.page = page;
    if (typeof search.event === "string" && search.event !== "") out.event = search.event;
    if (typeof search.status === "string" && search.status !== "") out.status = search.status;
    return out;
  },
  component: RepositoryDetailRoute,
});

function RepositoryDetailRoute() {
  const { me } = useAuth();
  return <RepositoryDetail me={me} />;
}
