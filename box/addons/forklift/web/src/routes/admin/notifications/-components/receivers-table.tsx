import { useNavigate } from "@tanstack/react-router";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable, type ColumnDef } from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";

import type { Receiver } from "@/services/v1/openapi-types";

export function ReceiversTable({ receivers }: { receivers: Receiver[] }) {
  const { t } = useTranslation();
  const navigate = useNavigate();

  const columns: ColumnDef<Receiver>[] = [
    {
      header: t("common.name"),
      accessorFn: (receiver) => receiver.name,
      cell: ({ row }) => <span className="whitespace-nowrap">{row.original.name}</span>,
    },
    {
      header: t("common.description"),
      accessorFn: (receiver) => receiver.description || "",
      cell: ({ row }) => (
        <span className="text-muted-foreground">{row.original.description || "-"}</span>
      ),
    },
    {
      header: t("common.webhook"),
      // The URL itself is write-only and never returned, so all the list can
      // say is whether one is set.
      accessorFn: (receiver) => (receiver.webhook_configured ? 1 : 0),
      cell: ({ row }) => (
        <span className="text-muted-foreground">
          {row.original.webhook_configured ? t("common.status.configured") : "-"}
        </span>
      ),
    },
    {
      header: t("common.created-by"),
      accessorFn: (receiver) => receiver.created_by || "",
      cell: ({ row }) => row.original.created_by || <span className="text-muted-foreground">-</span>,
    },
    {
      header: t("common.created"),
      accessorFn: (receiver) => receiver.created_at ?? "",
      cell: ({ row }) => (
        <span className="whitespace-nowrap text-muted-foreground">
          {row.original.created_at?.slice(0, 10)}
        </span>
      ),
    },
    {
      header: t("common.repositories"),
      accessorFn: (receiver) => receiver.repositories?.length ?? 0,
      cell: ({ row }) => {
        const repositories = row.original.repositories ?? [];

        return repositories.length === 0 ? (
          <span className="text-muted-foreground">0</span>
        ) : (
          <Badge variant="outline" title={repositories.join(", ")}>{repositories.length}</Badge>
        );
      },
    },
    {
      header: t("common.enabled"),
      accessorFn: (receiver) => (receiver.enabled ? 1 : 0),
      cell: ({ row }) =>
        row.original.enabled ? (
          <Badge variant="success">{t("common.status.enabled")}</Badge>
        ) : (
          <Badge variant="outline">{t("common.status.disabled")}</Badge>
        ),
    },
    {
      id: "actions",
      cell: ({ row }) => (
        <div className="flex min-w-0 items-center justify-end gap-2 max-sm:flex-wrap">
          <Button
            variant="outline"
            type="button"
            onClick={() =>
              navigate({ to: "/admin/notifications/$id", params: { id: String(row.original.id) } })
            }
          >
            {t("common.edit")}
          </Button>
        </div>
      ),
    },
  ];

  return (
    <DataTable
      columns={columns}
      data={receivers}
      empty={t("notification.empty")}
      rowTestId={(receiver) => `row-${receiver.name}`}
    />
  );
}
