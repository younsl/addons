import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Alert } from "@/components/app-ui/alert";
import { PageHeader } from "@/components/app-ui/page";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { useTranslation } from "@/lib/i18n";
import { CoverageShell, GroupTable, useCoverageOverview } from "./-dashboard";

export const Route = createFileRoute("/workspace/coverage/groups")({
  component: CoverageGroupsRoute,
});

// Coverage per GitLab group, worst first. Its own address so a group owner can
// be sent straight here rather than to the whole project list.
function CoverageGroupsRoute() {
  const { t } = useTranslation();
  const { data: overview, error, isLoading, scan, gitlab } = useCoverageOverview();
  const { data: groups = [] } = useQuery({
    ...openApiQueryOptions.listCoverageGroups(),
    enabled: !!overview,
  });

  if (isLoading) return <div className="p-4 text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (error || !overview) {
    return (
      <>
        <PageHeader title={t("coverage.title")} />
        <Alert>{t("coverage.unavailable")}</Alert>
      </>
    );
  }

  return (
    <CoverageShell overview={overview} scan={scan} gitlab={gitlab} active="groups">
      {groups.length === 0 ? (
        <p className="text-sm text-muted-foreground">{t("coverage.no-groups")}</p>
      ) : (
        <GroupTable groups={groups} gitlabURL={overview.gitlab_url} />
      )}
    </CoverageShell>
  );
}
