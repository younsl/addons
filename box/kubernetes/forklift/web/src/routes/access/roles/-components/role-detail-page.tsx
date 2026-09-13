import { useNavigate, useParams } from "@tanstack/react-router";
import { LockKeyhole } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { useTranslation } from "@/lib/i18n";
import { RoleAssignedUsersPanel } from "@/routes/access/roles/-components/role-assigned-users-panel";
import { RoleDangerPanel } from "@/routes/access/roles/-components/role-danger-panel";
import { RolePermissionsPanel } from "@/routes/access/roles/-components/role-permissions-panel";
import { useRoleDetail } from "@/routes/access/roles/-hooks/use-role-detail";

import type { Me } from "@/services/v1/openapi-types";

// Per-role modify page: permission mapping, assigned users, and the danger zone
// (delete). The Roles list is read-only; all edits happen here. The page is
// read-only (no add/remove permission, no delete) for an auditor and for managed
// roles, which are owned by the chart's declarative RBAC policy.
export function RoleDetailPage({ me }: { me: Me }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams({ strict: false }) as { id?: string };
  const roleId = Number(id);
  const { error, isLoading, members, role, runAction } = useRoleDetail(roleId);

  if (isLoading) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;
  // Both lists loaded and no role matched: the id in the URL is not a role.
  if (!role) return <Alert className="my-2.5">{error || "Role not found."}</Alert>;

  // Managed roles are reconciled from the chart's declarative RBAC policy and are
  // read-only via the API. Gate every edit control on !role.managed so an admin
  // never sees a button that would only return a 409; the backend still enforces
  // this regardless of the UI.
  const isEditable = Boolean(me.admin) && !role.managed;

  return (
    <div data-testid="page-role-detail">
      <PageHeader
        title={role.name}
        actions={
          <Button variant="outline" onClick={() => navigate({ to: "/access/roles" })}>
            {t("role.back")}
          </Button>
        }
      />
      {role.description && <PageDescription>{role.description}</PageDescription>}
      {role.managed && (
        <Card size="sm" className="mb-4 border-accent-ink/70">
          <CardContent>
            <h2 className="mb-2 flex items-center gap-2 text-base font-semibold">
              <LockKeyhole className="size-4 text-accent-ink" aria-hidden="true" />
              {t("role.managed")}
            </h2>
            <p className="m-0 text-sm leading-relaxed text-muted-foreground">
              {t("role.managed-note")}
            </p>
          </CardContent>
        </Card>
      )}
      {error && <Alert className="mb-4">{error}</Alert>}

      <RolePermissionsPanel role={role} canWrite={isEditable} runAction={runAction} />
      <RoleAssignedUsersPanel members={members} />
      {isEditable && <RoleDangerPanel role={role} runAction={runAction} />}
    </div>
  );
}
