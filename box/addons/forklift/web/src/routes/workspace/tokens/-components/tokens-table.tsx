import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { DataTable, type ColumnDef } from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import {
  describeTokenExpiry,
  stampMinute,
} from "@/routes/workspace/tokens/-utils/token-expiry";
import { formatTokenScope, parseTokenScopes } from "@/utils/token-scopes";

import type { Token } from "@/services/v1/openapi-types";

export function TokensTable({
  tokens,
  onEdit,
  onRevoke,
}: {
  tokens: Token[];
  onEdit: (token: Token) => void;
  onRevoke: (tokenId: number) => void;
}) {
  const { t, language } = useTranslation();

  const columns: ColumnDef<Token>[] = [
    {
      header: t("common.name"),
      accessorFn: (token) => token.name,
      cell: ({ row }) => row.original.name,
    },
    {
      header: t("common.description"),
      accessorFn: (token) => token.description || "",
      cell: ({ row }) => (
        <span className="text-muted-foreground">{row.original.description}</span>
      ),
    },
    {
      header: t("common.permissions"),
      // Sorted on the patterns alone; the actions vary per pattern and would
      // make the ordering read as arbitrary.
      accessorFn: (token) =>
        parseTokenScopes(token.scopes_json).map((scope) => scope.repo_pattern).join(","),
      cell: ({ row }) => (
        <>
          {parseTokenScopes(row.original.scopes_json).map((scope, index) => (
            <Badge key={index} className="mr-1 font-mono">
              {formatTokenScope(scope)}
            </Badge>
          ))}
        </>
      ),
    },
    {
      header: t("common.created"),
      accessorFn: (token) => token.created_at ?? "",
      cell: ({ row }) => (
        <span className="text-muted-foreground">{row.original.created_at?.slice(0, 10)}</span>
      ),
    },
    {
      header: t("common.expires"),
      accessorFn: (token) => token.expires_at ?? "",
      cell: ({ row }) => {
        const expiresAt = row.original.expires_at;
        if (!expiresAt) return <span className="text-muted-foreground">{t("common.never")}</span>;

        const expiry = describeTokenExpiry(expiresAt, language);

        return (
          <span className="text-muted-foreground">
            {expiresAt.slice(0, 10)}{" "}
            <span className={expiry.isExpired ? "text-destructive" : undefined}>
              ({expiry.isExpired ? t("common.expired") : expiry.label})
            </span>
          </span>
        );
      },
    },
    {
      header: t("common.last-used"),
      accessorFn: (token) => token.last_used_at ?? "",
      cell: ({ row }) => (
        <span className="tabular-nums text-muted-foreground">
          {row.original.last_used_at ? stampMinute(row.original.last_used_at) : t("common.never")}
        </span>
      ),
    },
    {
      id: "actions",
      cell: ({ row }) => (
        <div className="flex items-center justify-end gap-2">
          <Button variant="outline" onClick={() => onEdit(row.original)}>
            {t("common.edit")}
          </Button>
          <Button variant="destructive" onClick={() => onRevoke(row.original.id)}>
            {t("token.revoke")}
          </Button>
        </div>
      ),
    },
  ];

  return (
    <DataTable
      columns={columns}
      data={tokens}
      empty={t("token.empty")}
      rowTestId={(token) => `row-${token.name}`}
    />
  );
}
