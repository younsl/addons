import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { useTranslation } from "@/lib/i18n";
import { RepositoriesTable } from "@/routes/workspace/repositories/-components/repositories-table";
import { useRepositoriesList } from "@/routes/workspace/repositories/-hooks/use-repositories-list";
import { canViewRepositoryPolicy } from "@/utils/permissions";

import type { Me } from "@/services/v1/openapi-types";

export function RepositoriesPage({ me }: { me: Me }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const list = useRepositoriesList();

  return (
    <>
      <PageHeader
        title={t("common.repositories")}
        actions={me.admin && (
          <Button onClick={() => navigate({ to: "/workspace/repositories/new" })}>
            {t("repo.new")}
          </Button>
        )}
      />
      <PageDescription>{t("repo.list-description")}</PageDescription>
      {list.error && <Alert className="mb-4">{list.error}</Alert>}
      <RepositoriesTable
        byName={list.byName}
        // The security policy column is for admins and auditors; the upstream
        // column beside it is public.
        canViewSecurity={canViewRepositoryPolicy(me)}
        isEmpty={list.isEmpty}
        isExpanded={list.isExpanded}
        topLevel={list.topLevel}
        onToggleGroup={list.toggleGroup}
      />
    </>
  );
}
