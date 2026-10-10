import { useNavigate } from "@tanstack/react-router";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable, type ColumnDef } from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import { formatRolePermission } from "@/routes/access/roles/-utils/role-permissions";

import type { Role } from "@/services/v1/openapi-types";

export function RolesTable({ roles }: { roles: Role[] }) {
  const { t } = useTranslation();
  const navigate = useNavigate();

  const columns: ColumnDef<Role>[] = [
    {
      header: t("common.role"),
      accessorFn: (role) => role.name,
      cell: ({ row }) => row.original.name,
    },
    {
      header: t("common.source"),
      accessorFn: (role) => (role.managed ? 1 : 0),
      cell: ({ row }) => (
        <Badge
          title={row.original.managed
            ? "Managed by the declarative RBAC policy and not editable in the UI."
            : "Created in the UI or API and editable here."}
        >
          {row.original.managed ? t("common.status.managed") : t("common.status.local")}
        </Badge>
      ),
    },
    {
      header: t("common.description"),
      accessorFn: (role) => role.description || "",
      cell: ({ row }) => (
        <span className="text-muted-foreground">{row.original.description || "-"}</span>
      ),
    },
    {
      header: t("common.users"),
      accessorFn: (role) => role.user_count,
      // Read across screens: assigning a role on a user's page must show up here.
      cell: ({ row }) => (
        <span data-testid="value-user-count">{row.original.user_count}</span>
      ),
    },
    {
      header: t("common.permissions"),
      // Sorting on the patterns alone: the actions vary per pattern and would
      // make the ordering read as arbitrary.
      accessorFn: (role) => role.permissions.map((p) => p.repo_pattern).join(","),
      cell: ({ row }) => (
        <div className="flex flex-wrap gap-1.5">
          {row.original.permissions.map((permission) => (
            <Badge key={permission.id} className="font-mono">
              {formatRolePermission(permission)}
            </Badge>
          ))}
          {row.original.permissions.length === 0 && (
            <span className="text-muted-foreground">{t("common.none")}</span>
          )}
        </div>
      ),
    },
    {
      id: "actions",
      cell: ({ row }) => (
        <div className="text-right">
          <Button
            variant="outline"
            onClick={() =>
              navigate({ to: "/access/roles/$id", params: { id: String(row.original.id) } })
            }
          >
            {t("common.modify")}
          </Button>
        </div>
      ),
    },
  ];

  return (
    <DataTable
      columns={columns}
      data={roles}
      empty={t("role.empty")}
      // Named rows are how a parallel test finds its own without asserting a
      // total count that another worker can change underneath it.
      rowTestId={(role) => `row-${role.name}`}
    />
  );
}
