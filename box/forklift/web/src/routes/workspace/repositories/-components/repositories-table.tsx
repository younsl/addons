import { Link } from "@tanstack/react-router";

import { CopyIconButton } from "@/components/app-ui/copy-button";
import { Button } from "@/components/ui/button";
import { UpstreamStatus } from "@/components/feedback/upstream-status";
import {
  SortableHead,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
} from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import {
  ArtifactCount,
  CleanRatio,
  RepoSize,
  SecurityIcons,
} from "@/routes/workspace/repositories/-components/repository-metrics";
import { getRepositoryEndpoint } from "@/utils/repository-endpoint";

// The list endpoint returns more than the detail one: the aggregate counts
// this table shows are computed per listing and are absent everywhere else.
// RepositoryListItem is the document's name for that wider shape.
import type { RepositoryListItem } from "@/services/v1/openapi-types";

// Detail is read-only browsable by any authenticated user, so every name links
// into it; admin-only controls are hidden inside the detail page itself.
function RepositoryLink({ id, name }: { id: number; name: string }) {
  return (
    <Link to="/workspace/repositories/$id" params={{ id: String(id) }}>
      {name}
    </Link>
  );
}

export function RepositoriesTable({
  byName,
  canViewSecurity,
  isEmpty,
  isExpanded,
  topLevel,
  onToggleGroup,
}: {
  byName: Record<string, RepositoryListItem>;
  canViewSecurity: boolean;
  isEmpty: boolean;
  isExpanded: (id: number) => boolean;
  topLevel: RepositoryListItem[];
  onToggleGroup: (id: number) => void;
}) {
  const { t } = useTranslation();
  // Sorting applies to top-level rows; group members stay nested under their
  // group in config order. Unscanned repositories sink to the bottom on a Clean
  // sort - undefined, not zero, because nothing scanned is not the same as
  // nothing clean.
  const { sorted, sort } = useSort(topLevel, {
    name: (repo) => repo.name,
    format: (repo) => repo.format,
    type: (repo) => repo.type,
    visibility: (repo) => (repo.config.public ? "public" : "private"),
    artifacts: (repo) => repo.artifact_count ?? 0,
    size: (repo) => repo.total_size ?? 0,
    clean: (repo) => (repo.scanned_count ? (repo.clean_count ?? 0) / repo.scanned_count : undefined),
  });

  // The columns after Name, shared by top-level and nested rows so a group
  // member shows its own format, type, visibility and status.
  const cells = (repo: RepositoryListItem) => (
    <>
      <TableCell>{repo.format}</TableCell>
      <TableCell>{repo.type}</TableCell>
      <TableCell className="text-muted-foreground">
        {repo.config.public ? t("repo.public") : t("repo.private")}
      </TableCell>
      {/* The endpoint is a long URL that repeats the host on every row; the
          list shows just the copy action (full URL in the hover title and on
          the detail page). A proxy's cache switch lives on the detail page's
          settings tab - the column is too narrow to label it here. */}
      <TableCell
        className="whitespace-nowrap"
        title={getRepositoryEndpoint(repo.format, repo.name).url}
      >
        <CopyIconButton value={getRepositoryEndpoint(repo.format, repo.name).url} />
      </TableCell>
      <TableCell className="whitespace-nowrap"><ArtifactCount repo={repo} /></TableCell>
      <TableCell className="whitespace-nowrap"><RepoSize repo={repo} /></TableCell>
      <TableCell className="whitespace-nowrap"><CleanRatio repo={repo} /></TableCell>
      <TableCell>
        {repo.type === "proxy" ? (
          <UpstreamStatus repoId={repo.id} compact upstreamUrl={repo.upstream_url} />
        ) : (
          <span className="text-muted-foreground">-</span>
        )}
      </TableCell>
      <TableCell>
        {canViewSecurity ? <SecurityIcons repo={repo} /> : <span className="text-muted-foreground">-</span>}
      </TableCell>
    </>
  );

  return (
    <TableWrap>
      <Table className="min-w-[1200px] table-fixed">
        <TableHeader>
          <TableRow>
            <SortableHead k="name" sort={sort} className="w-[18%]">{t("common.name")}</SortableHead>
            <SortableHead k="format" sort={sort} className="w-[7%]">{t("common.format")}</SortableHead>
            <SortableHead k="type" sort={sort} className="w-[7%]">{t("common.type")}</SortableHead>
            <SortableHead k="visibility" sort={sort} className="w-[9%]">{t("repo.visibility")}</SortableHead>
            <TableHead className="w-[7%]">{t("common.endpoint")}</TableHead>
            <SortableHead k="artifacts" sort={sort} className="w-[10%]">{t("common.artifacts")}</SortableHead>
            <SortableHead k="size" sort={sort} className="w-[10%]">{t("common.size")}</SortableHead>
            <SortableHead k="clean" sort={sort} className="w-[11%]">{t("repo.clean-ratio")}</SortableHead>
            <TableHead className="w-[11%]">{t("common.upstream")}</TableHead>
            <TableHead className="w-[10%]">{t("common.security")}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {sorted.flatMap((repo) => {
            const isGroup = repo.type === "group";
            const members = repo.config.group?.members ?? [];
            const isOpen = isExpanded(repo.id);
            const rows = [
              <TableRow key={`r-${repo.id}`}>
                <TableCell className="overflow-hidden text-ellipsis whitespace-nowrap">
                  {isGroup ? (
                    <span className="flex min-w-0 items-center gap-1">
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-xs"
                        className="size-5 text-muted-foreground hover:text-foreground"
                        aria-expanded={isOpen}
                        aria-label={isOpen ? t("repo.collapse-group") : t("repo.expand-group")}
                        onClick={() => onToggleGroup(repo.id)}
                      >
                        {isOpen ? "▾" : "▸"}
                      </Button>
                      <RepositoryLink id={repo.id} name={repo.name} />
                      <span className="text-xs text-muted-foreground">({members.length})</span>
                    </span>
                  ) : (
                    <RepositoryLink id={repo.id} name={repo.name} />
                  )}
                </TableCell>
                {cells(repo)}
              </TableRow>,
            ];

            if (isGroup && isOpen) {
              members.forEach((name, index) => {
                const member = byName[name];
                const isLast = index === members.length - 1;

                rows.push(
                  <TableRow key={`r-${repo.id}-m-${name}`} className={cn("bg-muted/20", isLast && "last")}>
                    <TableCell className="overflow-hidden text-ellipsis whitespace-nowrap pl-7">
                      {member ? (
                        <RepositoryLink id={member.id} name={name} />
                      ) : (
                        <span className="text-muted-foreground">{name}</span>
                      )}
                    </TableCell>
                    {/* A group may name a repository that has since been
                        deleted; the row still says so rather than vanishing. */}
                    {member ? (
                      cells(member)
                    ) : (
                      <TableCell colSpan={9} className="text-muted-foreground">
                        {t("repo.member-not-found")}
                      </TableCell>
                    )}
                  </TableRow>,
                );
              });
            }

            return rows;
          })}
          {isEmpty && (
            <TableRow>
              <TableCell colSpan={10} className="text-muted-foreground">{t("repo.empty")}</TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </TableWrap>
  );
}
