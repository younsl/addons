import { useNavigate } from "@tanstack/react-router";
import { KeyRound } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable, type ColumnDef } from "@/components/app-ui/table";
import { useDateTime, useTranslation } from "@/lib/i18n";

import type { Me, User } from "@/services/v1/openapi-types";

export function UsersTable({ me, users }: { me: Me; users: User[] }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const navigate = useNavigate();

  const columns: ColumnDef<User>[] = [
    {
      header: t("common.username"),
      accessorFn: (user) => user.username,
      cell: ({ row }) => (
        <span className="whitespace-nowrap">
          {row.original.username}
          {row.original.username === me.username && (
            <Badge className="ml-2">{t("common.you")}</Badge>
          )}
        </span>
      ),
    },
    {
      header: t("common.type"),
      accessorFn: (user) => (user.robot ? 1 : 0),
      cell: ({ row }) => (
        <span className="whitespace-nowrap text-muted-foreground">
          {row.original.robot ? t("user.type-robot") : t("user.type-user")}
        </span>
      ),
    },
    {
      header: t("common.source"),
      accessorFn: (user) => user.source,
      cell: ({ row }) => <Badge>{row.original.source}</Badge>,
    },
    {
      header: t("common.email"),
      accessorFn: (user) => user.email || "",
      cell: ({ row }) => (
        <span className="text-muted-foreground">{row.original.email || "-"}</span>
      ),
    },
    {
      header: t("common.roles"),
      accessorFn: (user) => user.roles.map((role) => role.name).join(","),
      cell: ({ row }) => (
        <div className="flex flex-wrap gap-1.5">
          {row.original.roles.map((role) => (
            <Button
              key={role.id}
              variant="outline"
              size="xs"
              onClick={() =>
                navigate({ to: "/access/roles/$id", params: { id: String(role.id) } })
              }
            >
              {role.name}
            </Button>
          ))}
          {row.original.roles.length === 0 && (
            <span className="text-muted-foreground">{t("common.none")}</span>
          )}
        </div>
      ),
    },
    {
      header: t("common.token"),
      accessorFn: (user) => user.token_count,
      cell: ({ row }) => (
        <span className="inline-flex items-center gap-1.5 whitespace-nowrap text-muted-foreground">
          <KeyRound className="size-3.5" aria-hidden="true" />
          {/* Read across screens: creating or revoking a token on the user's
              detail page must show up here. */}
          <span data-testid="value-token-count">{row.original.token_count}</span>
        </span>
      ),
    },
    {
      header: t("common.status"),
      accessorFn: (user) => (user.disabled ? 1 : 0),
      cell: ({ row }) => (
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
          <span
            className={
              row.original.disabled
                ? "size-2 rounded-full bg-destructive"
                : "size-2 rounded-full bg-[var(--fx-success)]"
            }
          />{" "}
          {row.original.disabled ? t("common.status.disabled") : t("common.status.active")}
        </span>
      ),
    },
    {
      header: t("common.last-login"),
      accessorFn: (user) => user.last_login_at ?? "",
      cell: ({ row }) =>
        row.original.robot ? (
          // Robot accounts cannot sign in, so "never" would be misleading here.
          <span className="whitespace-nowrap italic text-muted-foreground">
            {t("user.no-login")}
          </span>
        ) : (
          <span
            className="whitespace-nowrap text-muted-foreground"
            title={row.original.last_login_at ?? undefined}
          >
            {row.original.last_login_at ? fmtDate(row.original.last_login_at) : t("common.never")}
          </span>
        ),
    },
    {
      id: "actions",
      cell: ({ row }) => (
        <div className="text-right">
          <Button
            variant="outline"
            onClick={() =>
              navigate({ to: "/access/users/$id", params: { id: String(row.original.id) } })
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
      data={users}
      empty={t("user.empty")}
      rowTestId={(user) => `row-${user.username}`}
    />
  );
}
