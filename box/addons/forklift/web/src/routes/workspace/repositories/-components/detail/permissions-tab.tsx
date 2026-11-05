import { useQuery } from "@tanstack/react-query";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { Link } from "@tanstack/react-router";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { SortableHead, Table, TableBody, TableCell, TableHeader, TableRow, TableWrap, useSort } from "@/components/app-ui/table";
import { Card, CardContent } from "@/components/ui/card";
import { useTranslation } from "@/lib/i18n";

export function RepoPermissions({ repoId }: { repoId: number }) {
  const { t } = useTranslation();
  // Two independent reads: which roles grant access, and which tokens already
  // hold it. Either can fail on its own, and one failing is not a reason to
  // hide the other.
  const permissionsQuery = useQuery({
    ...openApiQueryOptions.listRepositoryPermissions({ path: { id: repoId } }),
    meta: { suppressGlobalErrorToast: true },
  });
  const tokensQuery = useQuery({
    ...openApiQueryOptions.listRepositoryTokens({ path: { id: repoId } }),
    meta: { suppressGlobalErrorToast: true },
  });
  const perms = permissionsQuery.data;
  const tokens = tokensQuery.data;
  const error =
    getErrorMessageIfAny(permissionsQuery.error) || getErrorMessageIfAny(tokensQuery.error);
  const { sorted: sortedPerms, sort: permSort } = useSort(perms ?? [], {
    role: (p) => p.role,
    pattern: (p) => p.repo_pattern,
    actions: (p) => p.actions.join(","),
    users: (p) => p.user_count,
  });
  const { sorted: sortedTokens, sort: tokenSort } = useSort(tokens ?? [], {
    token: (tok) => tok.name,
    owner: (tok) => tok.owner,
    scope: (tok) => (tok.unscoped ? "" : tok.repo_pattern),
    actions: (tok) => (tok.unscoped ? "" : tok.actions.join(",")),
    expires: (tok) => tok.expires_at ?? "",
  });

  return (
    <>
      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">
          {t("common.roles")} <span className="text-xs font-normal text-muted-foreground">{t("repo.access-subtitle")}</span>
        </h2>
        {error && <Alert className="mb-4">{error}</Alert>}
        {!perms ? <div className="text-sm text-muted-foreground">{t("common.loading")}</div> : perms.length === 0 ? (
          <p className="m-0 text-sm text-muted-foreground">{t("repo.no-access-role")}</p>
        ) : (
          <TableWrap>
          <Table>
            <TableHeader>
              <TableRow><SortableHead k="role" sort={permSort}>{t("common.role")}</SortableHead><SortableHead k="pattern" sort={permSort}>{t("repo.matched-pattern")}</SortableHead><SortableHead k="actions" sort={permSort}>{t("common.actions")}</SortableHead><SortableHead k="users" sort={permSort}>{t("common.users")}</SortableHead></TableRow>
            </TableHeader>
            <TableBody>
              {sortedPerms.map((p, i) => (
                <TableRow key={`${p.role_id}-${i}`}>
                  <TableCell><Link to="/access/roles/$id" params={{ id: String(p.role_id) }}>{p.role}</Link></TableCell>
                  <TableCell className="font-mono text-xs">{p.repo_pattern}</TableCell>
                  <TableCell>
                    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                      {p.actions.map((a) => <Badge key={a}>{a}</Badge>)}
                    </div>
                  </TableCell>
                  <TableCell>{p.user_count}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          </TableWrap>
        )}
        </CardContent>
      </Card>

      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">
          {t("token.title")} <span className="text-xs font-normal text-muted-foreground">{t("repo.tokens-subtitle")}</span>
        </h2>
        {!tokens ? <div className="text-sm text-muted-foreground">{t("common.loading")}</div> : tokens.length === 0 ? (
          <p className="m-0 text-sm text-muted-foreground">{t("repo.no-access-token")}</p>
        ) : (
          <TableWrap>
          <Table>
            <TableHeader>
              <TableRow><SortableHead k="token" sort={tokenSort}>{t("common.token")}</SortableHead><SortableHead k="owner" sort={tokenSort}>{t("common.owner")}</SortableHead><SortableHead k="scope" sort={tokenSort}>{t("common.scope")}</SortableHead><SortableHead k="actions" sort={tokenSort}>{t("common.actions")}</SortableHead><SortableHead k="expires" sort={tokenSort}>{t("common.expires")}</SortableHead></TableRow>
            </TableHeader>
            <TableBody>
              {sortedTokens.map((tok) => (
                <TableRow key={tok.token_id}>
                  <TableCell>{tok.name}</TableCell>
                  <TableCell>{tok.owner || <span className="text-muted-foreground">{t("common.unknown")}</span>}</TableCell>
                  <TableCell className="font-mono text-xs">
                    {tok.unscoped
                      ? <span className="text-muted-foreground" title="Unscoped token: inherits the owner's role access to every repository.">{t("token.unscoped")}</span>
                      : tok.repo_pattern}
                  </TableCell>
                  <TableCell>
                    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                      {tok.unscoped
                        ? <span className="text-muted-foreground">{t("token.per-owner-roles")}</span>
                        : tok.actions.map((a) => <Badge key={a}>{a}</Badge>)}
                    </div>
                  </TableCell>
                  <TableCell className="text-muted-foreground">{tok.expires_at ? new Date(tok.expires_at).toLocaleDateString() : t("common.never")}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          </TableWrap>
        )}
        </CardContent>
      </Card>
    </>
  );
}

// formatStatTime renders an absolute local timestamp including the viewer's
// timezone, formatted in the app's selected language (en/ko) rather than the
// browser locale, so the Statistics clock matches the chosen UI language.
