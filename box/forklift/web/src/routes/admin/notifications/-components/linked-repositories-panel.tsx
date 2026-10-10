import { useQuery } from "@tanstack/react-query";

import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// The repositories whose notify config selects this receiver. Read-only: the
// link is made on the repository's own settings, and it is shown here so the
// disabled delete button explains itself - the API returns 409 while any
// remain.
export function LinkedRepositoriesPanel({ repositories }: { repositories: string[] }) {
  const { t } = useTranslation();
  // Joined by name for the format and type columns. Best-effort: the link is
  // the receiver's own field, so a repository missing from the list - or a
  // list this account may not read in full - renders a dash rather than
  // hiding the row.
  const repositoriesQuery = useQuery(openApiQueryOptions.listRepositories());
  const byName = new Map((repositoriesQuery.data ?? []).map((repo) => [repo.name, repo]));

  return (
    <div className="mt-5" data-testid="panel-linked-repositories">
      <h2 className="mb-1 flex items-baseline gap-2 text-base font-semibold">
        {t("notification.linked-repos")}
        {/* Read across screens: selecting this receiver in a repository's notify
            settings must show up here. */}
        <span
          className="text-sm font-normal text-muted-foreground"
          data-testid="value-linked-repositories"
        >
          {repositories.length}
        </span>
      </h2>
      <p className="mb-3 mt-0 text-sm text-muted-foreground">
        {t("notification.linked-repos-hint")}
      </p>
      {repositories.length === 0 ? (
        <p className="text-sm text-muted-foreground">{t("notification.no-linked-repos")}</p>
      ) : (
        <TableWrap>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t("common.repository")}</TableHead>
                <TableHead>{t("common.format")}</TableHead>
                <TableHead>{t("common.type")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {repositories.map((name) => {
                const repository = byName.get(name);
                return (
                  <TableRow key={name}>
                    <TableCell className="font-mono text-xs">{name}</TableCell>
                    <TableCell>{repository?.format ?? "-"}</TableCell>
                    <TableCell>{repository?.type ?? "-"}</TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </TableWrap>
      )}
    </div>
  );
}
