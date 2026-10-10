import { Link, useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { SeverityBadge } from "@/components/app-ui/severity-badge";
import { Switch } from "@/components/ui/switch";
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
import { useAuth } from "@/authContext";
import { useTranslation } from "@/lib/i18n";
import { useBulkApproval } from "@/routes/workspace/approvals/-hooks/use-bulk-approval";
import { canReviewApprovals } from "@/utils/permissions";

// BulkApprovalPage is the dedicated bulk-approve screen: one actionable row per
// repository that has a pending queue, showing how many packages are pending
// and how many are Clean. A single Clean-only toggle decides whether an approve
// clears the whole queue or only the packages with no known advisories, so the
// operator sees the exact blast radius before acting.
export function BulkApprovalPage() {
  const { t } = useTranslation();
  const { me } = useAuth();
  const navigate = useNavigate();
  const bulk = useBulkApproval();
  // Auditors may view the bulk screen but not act; only approvers and admins
  // decide.
  const canDecide = canReviewApprovals(me);

  const rows = bulk.repos;
  const total = rows.reduce(
    (sum, row) => sum + (bulk.cleanOnly ? row.clean : row.pending),
    0,
  );
  const { sorted, sort } = useSort(rows, {
    repo: (row) => row.repo_name,
    format: (row) => row.format,
    type: (row) => row.type,
    pending: (row) => row.pending,
    clean: (row) => row.clean,
  });

  return (
    <div data-testid="page-bulk-approval">
      <PageHeader
        title={t("approval.bulk")}
        actions={
          <Button variant="outline" onClick={() => navigate({ to: "/workspace/approvals" })}>
            {t("approval.back")}
          </Button>
        }
      />
      <PageDescription>{t("approval.bulk-description")}</PageDescription>

      {!canDecide && <Alert className="mb-5">{t("approval.read-only")}</Alert>}

      <div className="space-y-5">
        {/* One shared decision: approve everything, or only the Clean packages.
            The counts and the per-row button both follow this toggle. */}
        <div className="flex min-w-0 flex-col gap-4 rounded-lg border border-border bg-card p-4 sm:flex-row sm:items-end sm:justify-between">
          <label className="flex min-w-0 items-start gap-3">
            <Switch
              checked={bulk.cleanOnly}
              onCheckedChange={(checked) => bulk.setCleanOnly(checked === true)}
              className="mt-0.5 shrink-0"
            />
            <span className="min-w-0">
              <span className="block text-sm font-medium">{t("approval.clean-only")}</span>
              <span className="block text-xs leading-relaxed text-muted-foreground">
                {t("approval.clean-only-hint")}
              </span>
            </span>
          </label>
          <Field className="w-full sm:w-[280px]">
            <FieldLabel>
              {t("approval.note-optional")} <span className="text-destructive">*</span>
            </FieldLabel>
            {/* Required here though optional elsewhere: a bulk decision covers
                packages nobody looked at individually, so the reason is the
                only record of why. */}
            <Input
              value={bulk.note}
              placeholder={t("approval.reason-placeholder")}
              required
              disabled={!canDecide}
              aria-invalid={!bulk.note.trim()}
              onChange={(event) => bulk.setNote(event.target.value)}
            />
            <span className="text-xs text-muted-foreground">{t("approval.comment-required")}</span>
          </Field>
        </div>

        {bulk.error && <Alert>{bulk.error}</Alert>}
        {bulk.doneMessage && (
          <div className="rounded-md border border-[var(--success)]/40 bg-[var(--success)]/10 px-3 py-2 text-sm text-foreground">
            {bulk.doneMessage}
          </div>
        )}

        {bulk.isLoading ? (
          <div className="text-sm text-muted-foreground">{t("common.loading")}</div>
        ) : rows.length === 0 ? (
          <div className="rounded-md border border-dashed border-[var(--fx-border-subtle)] px-3 py-10 text-center text-sm text-muted-foreground">
            {t("approval.no-queue")}
          </div>
        ) : (
          <TableWrap>
            <Table className="min-w-[680px] table-fixed">
              <TableHeader>
                <TableRow>
                  <SortableHead k="repo" sort={sort}>{t("common.repository")}</SortableHead>
                  <SortableHead k="format" sort={sort} className="w-[13%]">{t("common.format")}</SortableHead>
                  <SortableHead k="type" sort={sort} className="w-[12%]">{t("common.type")}</SortableHead>
                  <SortableHead k="pending" sort={sort} className="w-[12%] text-right">{t("common.pending")}</SortableHead>
                  <SortableHead k="clean" sort={sort} className="w-[12%] text-right">{t("approval.clean-short")}</SortableHead>
                  <TableHead className="w-[160px] text-right"></TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {sorted.map((row) => {
                  const count = bulk.cleanOnly ? row.clean : row.pending;

                  return (
                    <TableRow key={row.repo_name} data-testid={`row-${row.repo_name}`}>
                      <TableCell className="truncate font-medium" title={row.repo_name}>
                        {row.id ? (
                          <Link
                            to="/workspace/repositories/$id/$tab"
                            params={{ id: String(row.id), tab: "approvals" }}
                          >
                            {row.repo_name}
                          </Link>
                        ) : (
                          row.repo_name
                        )}
                      </TableCell>
                      <TableCell>{row.format}</TableCell>
                      <TableCell>{row.type}</TableCell>
                      <TableCell className="text-right tabular-nums">{row.pending}</TableCell>
                      <TableCell className="text-right tabular-nums">
                        {row.clean > 0
                          ? <SeverityBadge severity="none">{row.clean}</SeverityBadge>
                          : <span className="text-muted-foreground">0</span>}
                      </TableCell>
                      <TableCell className="text-right">
                        <Button
                          className="w-[140px]"
                          disabled={
                            !canDecide ||
                            bulk.busyRepo !== null ||
                            count === 0 ||
                            !bulk.note.trim()
                          }
                          title={
                            !canDecide
                              ? t("approval.read-only")
                              : !bulk.note.trim()
                                ? t("approval.comment-required")
                                : undefined
                          }
                          onClick={() => bulk.approve(row.repo_name)}
                        >
                          {bulk.busyRepo === row.repo_name
                            ? "Approving…"
                            : count === 0
                              ? (bulk.cleanOnly ? "No Clean" : "Nothing")
                              : `Approve ${count}${bulk.cleanOnly ? " Clean" : ""}`}
                        </Button>
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </TableWrap>
        )}

        {rows.length > 0 && (
          <p className="text-sm text-muted-foreground">
            {bulk.cleanOnly
              ? `${total} Clean ${total === 1 ? "package" : "packages"} across ${rows.length} ${rows.length === 1 ? "repository" : "repositories"} would be approved; vulnerable and unscanned packages stay pending.`
              : `${total} pending ${total === 1 ? "package" : "packages"} across ${rows.length} ${rows.length === 1 ? "repository" : "repositories"}. Approving serves them (age policy still applies) and cannot be undone in bulk.`}
          </p>
        )}
      </div>
    </div>
  );
}
