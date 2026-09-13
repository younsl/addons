import { useNavigate } from "@tanstack/react-router";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { ApprovalStatusBadge } from "@/components/app-ui/status-badge";
import { SeverityBar, sevRank } from "@/components/app-ui/severity-bar";
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
import { highlightMatches } from "@/components/app-ui/table-search";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { repoLink, userLink } from "@/routes/workspace/approvals/-utils/entity-links";

import type { PackageApproval } from "@/services/v1/openapi-types";

export function ApprovalQueueTable({
  rows,
  showRepo,
  highlightRe,
  repoIds,
  userIds,
}: {
  rows: PackageApproval[];
  showRepo: boolean;
  highlightRe: RegExp | null;
  repoIds: Record<string, number>;
  userIds: Record<string, number>;
}) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const navigate = useNavigate();
  const { sorted, sort } = useSort(rows, {
    repo: (row) => row.repo_name,
    package: (row) => row.package,
    version: (row) => row.last_requested_version,
    vuln: (row) => sevRank(row.vuln_severity),
    requestedBy: (row) => row.requested_by,
    requests: (row) => row.request_count,
    lastRequested: (row) => row.last_requested_at,
    // Pending rows have no decision yet; useSort sinks empty values to the
    // bottom in both directions, so they never crowd out the decided ones.
    decidedAt: (row) => row.decided_at ?? "",
    status: (row) => row.status,
  });

  return (
    <TableWrap>
      <Table className="min-w-[1040px] table-fixed">
        <TableHeader>
          <TableRow>
            {showRepo && (
              <SortableHead k="repo" sort={sort} className="w-[13%]">
                {t("common.repository")}
              </SortableHead>
            )}
            <SortableHead k="package" sort={sort} className="w-[17%]">{t("common.package")}</SortableHead>
            <SortableHead k="version" sort={sort} className="w-[9%]">{t("common.version")}</SortableHead>
            <SortableHead k="vuln" sort={sort} className="w-[8%]">{t("common.vuln")}</SortableHead>
            <SortableHead k="requestedBy" sort={sort} className="w-[11%]">{t("approval.requested-by")}</SortableHead>
            <SortableHead k="requests" sort={sort} className="w-[6%]">{t("common.requests")}</SortableHead>
            <SortableHead k="lastRequested" sort={sort} className="w-[11%]">{t("approval.last-requested")}</SortableHead>
            <SortableHead k="decidedAt" sort={sort} className="w-[11%]">{t("approval.decided-at")}</SortableHead>
            <SortableHead k="status" sort={sort} className="w-[8%]">{t("common.status")}</SortableHead>
            <TableHead className="w-[72px] text-right"></TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {sorted.map((row) => (
            <TableRow key={row.id} data-testid={`row-${row.package}`}>
              {showRepo && (
                <TableCell className="truncate" title={row.repo_name}>
                  {repoLink(row.repo_name, repoIds, highlightMatches(row.repo_name, highlightRe))}
                </TableCell>
              )}
              <TableCell className="max-w-[260px] truncate font-mono text-xs" title={row.package}>
                {highlightMatches(row.package, highlightRe)}
              </TableCell>
              <TableCell className="font-mono text-xs">
                {row.last_requested_version
                  ? highlightMatches(row.last_requested_version, highlightRe)
                  : <span className="text-muted-foreground">{t("common.unknown")}</span>}
              </TableCell>
              <TableCell>
                <SeverityBar
                  severity={row.vuln_severity}
                  counts={row.vuln_counts}
                  scope={row.vuln_scope}
                  source={row.vuln_source}
                  scannedAt={row.vuln_scanned_at}
                  advisories={row.vuln_advisories}
                />
              </TableCell>
              <TableCell className="truncate" title={row.requested_by || t("common.anonymous")}>
                {row.requested_by
                  ? userLink(row.requested_by, userIds, highlightMatches(row.requested_by, highlightRe))
                  : <span className="text-muted-foreground">{t("common.anonymous")}</span>}
              </TableCell>
              <TableCell className="tabular-nums">
                {highlightMatches(String(row.request_count), highlightRe)}
              </TableCell>
              <TableCell
                className="truncate text-muted-foreground"
                title={fmtDate(row.last_requested_at)}
              >
                {highlightMatches(fmtDate(row.last_requested_at), highlightRe)}
              </TableCell>
              {/* Pending requests have no decision time yet; a dash keeps the
                  column readable instead of leaving a blank cell. */}
              <TableCell
                className="truncate text-muted-foreground"
                title={row.decided_at
                  ? `${fmtDate(row.decided_at)}${row.decided_by ? ` (${row.decided_by})` : ""}`
                  : ""}
              >
                {row.decided_at ? highlightMatches(fmtDate(row.decided_at), highlightRe) : "-"}
              </TableCell>
              <TableCell>
                <div className="flex flex-wrap items-center gap-1">
                  <ApprovalStatusBadge
                    status={row.status}
                    title={row.note ? `${row.decided_by}: ${row.note}` : row.decided_by}
                  />
                  {row.notified_receivers && row.notified_receivers.length > 0 && (
                    <Badge variant="secondary" title={row.notified_receivers.join(", ")}>
                      {t("approval.noted")}
                    </Badge>
                  )}
                </div>
              </TableCell>
              <TableCell className="whitespace-nowrap text-right">
                <Button
                  onClick={() =>
                    navigate({ to: "/workspace/approvals/$id", params: { id: String(row.id) } })
                  }
                >
                  {t("common.review")}
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </TableWrap>
  );
}
