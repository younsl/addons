import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { RolesTable } from "@/routes/access/roles/-components/roles-table";
import { useRolesList } from "@/routes/access/roles/-hooks/use-roles-list";

import type { Me } from "@/services/v1/openapi-types";

// Admin role directory (read-only). Roles and their permissions are defined on
// /access/roles/new and edited on /access/roles/$id; this page only displays
// them.
export function RolesPage({ me }: { me: Me }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const rolesQuery = useRolesList();
  const roles = rolesQuery.data ?? [];
  const error = getErrorMessageIfAny(rolesQuery.error);

  return (
    <div data-testid="page-roles">
      <PageHeader
        title={
          <span className="flex items-baseline gap-2">
            {t("common.roles")}
            <span className="text-base font-normal text-muted-foreground">{roles.length}</span>
          </span>
        }
        actions={me.admin && (
          <Button onClick={() => navigate({ to: "/access/roles/new" })}>
            {t("role.create")}
          </Button>
        )}
      />
      <PageDescription>{t("role.list-description")}</PageDescription>
      {error && <Alert className="mb-4">{error}</Alert>}

      <RolesTable roles={roles} />
    </div>
  );
}
