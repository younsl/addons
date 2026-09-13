import { useEffect, useState, type ReactNode } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/app-ui/select";
import { TablePager, TableSearchControls, useTableSearch } from "@/components/app-ui/table-search";
import { useTranslation } from "@/lib/i18n";
import { ApprovalQueueTable } from "@/routes/workspace/approvals/-components/approval-queue-table";
import {
  APPROVALS_PAGE_SIZE,
  APPROVAL_STATUSES,
  useApprovalQueue,
  useUserIdsByUsername,
} from "@/routes/workspace/approvals/-hooks/use-approval-queue";

// ApprovalList renders the approval queue scoped to an optional repository:
// status filter, table, pagination. Shared by the global Approvals page and the
// repository detail's Approvals tab.
//
// There is no reload prop any more. The mutations invalidate the queue, so a
// decision made anywhere - this page, the detail page, the bulk screen -
// refreshes every view of it, including the ones on other filters.
export function ApprovalList({
  repo = "",
  showRepo = true,
  filters,
  repoNames = [],
  repoIds = {},
}: {
  repo?: string;
  showRepo?: boolean;
  filters?: ReactNode;
  // Repository names the bulk-approve button is offered for. Empty hides it.
  repoNames?: string[];
  // Maps repository name to id so the Repository column can link to its detail.
  repoIds?: Record<string, number>;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [status, setStatus] = useState<string>("pending");
  const search = useTableSearch();
  const { count, error, pendingCount, rows } = useApprovalQueue({
    repo,
    status,
    page: search.page,
    q: search.q,
    regex: search.regex,
  });
  const userIds = useUserIdsByUsername();

  // Changing the repository scope restarts paging: page 3 of one repository's
  // queue is rarely page 3 of another's, and an out-of-range page shows nothing.
  const { setPage } = search;
  useEffect(() => { setPage(0); }, [repo, setPage]);

  return (
    <section aria-labelledby="approval-queue-title">
      <div className="mb-3 flex min-w-0 items-end justify-between gap-3 max-sm:flex-col max-sm:items-stretch">
        <div className="min-w-0">
          <h2 id="approval-queue-title" className="m-0 flex items-baseline gap-2 text-base font-semibold">
            {t("approval.queue")}
            <span
              data-testid="value-approval-count"
              className="text-sm font-normal text-muted-foreground"
            >
              {count.toLocaleString()}
            </span>
          </h2>
          <p className="m-0 mt-1 text-sm leading-6 text-muted-foreground">
            {t("approval.description")}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2 text-sm text-muted-foreground max-sm:justify-between">
          <span data-testid="value-pending-count">{pendingCount.toLocaleString()} pending</span>
        </div>
      </div>

      <div className="mb-3 flex min-w-0 items-center gap-2 max-sm:flex-col max-sm:items-stretch">
        <div className="flex min-w-0 flex-1 items-center gap-2 max-sm:flex-col max-sm:items-stretch">
          <Select
            className="w-full sm:w-[160px]"
            value={status}
            onChange={(next) => { setStatus(next); search.setPage(0); }}
            options={[
              ...APPROVAL_STATUSES.map((value) => ({ value, label: value })),
              { value: "", label: "all statuses" },
            ]}
          />
          {filters}
          <TableSearchControls search={search} />
        </div>
        {/* Bulk approval opens on its own page (one actionable row per repo with
            a pending queue). Hidden only when there are no targetable repos. */}
        {repoNames.length > 0 && (
          <Button
            className="shrink-0"
            disabled={pendingCount === 0}
            title={pendingCount === 0 ? "No pending approvals" : undefined}
            onClick={() => navigate({ to: "/workspace/approvals/bulk" })}
          >
            {t("approval.bulk")}
          </Button>
        )}
      </div>
      {search.regexError && <Alert className="mb-4">{t("common.invalid-regex")}</Alert>}
      {error && <Alert className="mb-4">{error}</Alert>}
      {rows.length === 0 ? (
        <div className="rounded-md border border-dashed border-[var(--fx-border-subtle)] px-3 py-8 text-center text-sm text-muted-foreground">
          {search.q ? t("common.no-search-matches") : `No ${status || "approval"} requests.`}
        </div>
      ) : (
        <ApprovalQueueTable
          rows={rows}
          showRepo={showRepo}
          highlightRe={search.highlightRe}
          repoIds={repoIds}
          userIds={userIds}
        />
      )}
      <TablePager
        page={search.page}
        pageSize={APPROVALS_PAGE_SIZE}
        total={count}
        onPage={search.setPage}
      />
    </section>
  );
}
