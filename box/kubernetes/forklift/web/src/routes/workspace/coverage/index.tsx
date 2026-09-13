import { useMemo, type MouseEvent, type ReactNode } from "react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { VolumeX } from "lucide-react";
import { Badge } from "@/components/app-ui/badge";
import { CopyOnHover } from "@/components/app-ui/copy-button";
import { Alert } from "@/components/app-ui/alert";
import { CoverageTrendChart } from "@/components/app-ui/coverage-trend-chart";
import { PageHeader } from "@/components/app-ui/page";
import {
  SortableHead,
  Table,
  TableBody,
  TableCell,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
} from "@/components/app-ui/table";
import { highlightMatches, useTableSearch, TableSearchControls } from "@/components/app-ui/table-search";
import { Card, CardContent } from "@/components/ui/card";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import type { CoverageOverview, CoverageProject } from "@/services/v1/openapi-types";
import { useLanguage, useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { isMuted, readExclusionReason } from "@/lib/coverage-exclusion";
import { formatAbsolute, formatRelative } from "@/utils/format-relative";
import { CoverageShell, useCoverageOverview, WiringMark } from "./-dashboard";

// Status filter values. "all" is the table's default; the rest match the
// verdict on a project, plus the two out-of-scope buckets which are not
// verdicts at all but are the questions people ask of the table next.
type StatusFilter = "all" | "yes" | "partial" | "no" | "error" | "skipped" | "excluded";

const STATUS_FILTERS: StatusFilter[] = [
  "all",
  "yes",
  "partial",
  "no",
  "error",
  "skipped",
  "excluded",
];

export const Route = createFileRoute("/workspace/coverage/")({
  // status is the filter, held in the URL rather than in component state so the
  // view can be linked to and the back button moves between filters. The alarm
  // uses the same parameter to point at the projects its report was about.
  validateSearch: (search: Record<string, unknown>): { status?: StatusFilter } =>
    STATUS_FILTERS.includes(search.status as StatusFilter)
      ? { status: search.status as StatusFilter }
      : {},
  component: CoverageListRoute,
});

// The trend and the project table: what has happened over time, and what is
// left to do. The per-group breakdown is its own view.
function CoverageListRoute() {
  const { t } = useTranslation();
  const { status = "all" } = Route.useSearch();
  const tableSearch = useTableSearch();

  const { data: overview, error, isLoading, scan, gitlab } = useCoverageOverview();
  // No days parameter: the server's default is the retention it actually keeps,
  // so the chart cannot promise a window the data does not cover.
  const { data: history = [] } = useQuery({
    ...openApiQueryOptions.listCoverageHistory(),
    enabled: !!overview,
  });

  if (isLoading) return <div className="p-4 text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (error || !overview) {
    return (
      <>
        <PageHeader title={t("coverage.title")} />
        <Alert>{t("coverage.unavailable")}</Alert>
      </>
    );
  }

  return (
    <CoverageShell overview={overview} scan={scan} gitlab={gitlab} active="list">
      {history.length > 0 && (
        <Card className="mb-4">
          <CardContent className="pt-4">
            <h2 className="mb-3 text-sm font-medium">{t("coverage.trend-title")}</h2>
            <CoverageTrendChart snapshots={history} retentionDays={overview.history_retention_days} />
          </CardContent>
        </Card>
      )}
      <ProjectTable overview={overview} status={status} tableSearch={tableSearch} />
    </CoverageShell>
  );
}

// The LED colour per verdict: done, started, not started, unknown.
const appliedLed: Record<string, string> = {
  yes: "bg-[var(--fx-success)]",
  partial: "bg-[var(--fx-warning)]",
  no: "bg-[var(--fx-danger)]",
  error: "bg-[var(--fx-danger)]",
};

// StatusLed is a dot plus a word. The dot carries the verdict at a glance down a
// column; the word is what makes it readable, and what a screen reader gets.
function StatusLed({ tone, children }: { tone: string; children: ReactNode }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5 whitespace-nowrap">
      <span
        className={cn("size-2 shrink-0 rounded-full", appliedLed[tone] ?? "bg-muted-foreground/50")}
        aria-hidden="true"
      />
      <span className="min-w-0 truncate">{children}</span>
    </span>
  );
}

// The Status column names who muted the project. The topic that did it belongs
// on the detail page, which has room for it, so the column takes the short form.
function exclusionLabel(reason: string, t: (key: MessageKey) => string): string {
  const parsed = readExclusionReason(reason);
  if (!parsed) return reason;
  if (parsed.key === "coverage.muted-by-console") return t("coverage.muted-title");
  if (parsed.key === "coverage.muted-by-topic") return t("coverage.muted-by-topic-short");
  return t(parsed.key);
}

// INTERACTIVE_SELECTOR names the elements that handle their own clicks. The row
// navigates, so it has to yield to every one of them: without this the copy
// control opens the project instead of copying, and the next control added to a
// cell breaks the same way with nobody noticing until they use it.
const INTERACTIVE_SELECTOR = [
  "a",
  "button",
  "input",
  "select",
  "textarea",
  "label",
  '[role="button"]',
  '[role="link"]',
  '[role="checkbox"]',
  '[role="menuitem"]',
].join(", ");

// shouldIgnoreRowClick reports the clicks on a row that are not a request to
// open the project. Each case is a real way the shortcut goes wrong:
//
//   - a control inside the row already acted on the click, or asked for it to
//     be left alone by preventing the default,
//   - the click carries a modifier or came from a secondary button, so the
//     reader means "open elsewhere" or "extend the selection", and navigating
//     in place would take the page out from under them,
//   - the click ended a text selection made inside this row, which is somebody
//     copying a value, not opening it. The selection has to be inside this row:
//     one left behind elsewhere on the page must not make rows unclickable.
function shouldIgnoreRowClick(event: MouseEvent<HTMLElement>): boolean {
  if (event.defaultPrevented) return true;
  if (event.button !== 0) return true;
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return true;
  if ((event.target as HTMLElement).closest(INTERACTIVE_SELECTOR)) return true;

  const selection = window.getSelection();
  return Boolean(
    selection &&
      !selection.isCollapsed &&
      selection.toString().trim() !== "" &&
      selection.anchorNode &&
      event.currentTarget.contains(selection.anchorNode)
  );
}

// statusRank orders the status column by how much work is left, which is what
// the column is scanned for. Out-of-scope rows sort last in either direction:
// they are not a verdict, so they do not belong among them.
function statusRank(p: CoverageProject): number {
  if (p.exclude_reason) return 6;
  if (p.skipped) return 5;
  switch (p.applied) {
    case "no":
      return 0;
    case "partial":
      return 1;
    case "error":
      return 2;
    default:
      return 3;
  }
}

// The verdict values are the wire format ("yes"/"partial"/...), which is not
// what a reader should see; each maps to its own translated label.
const appliedLabelKey: Record<string, MessageKey> = {
  yes: "coverage.applied-yes",
  partial: "coverage.applied-partial",
  no: "coverage.applied-no",
  error: "coverage.applied-error",
};

function ProjectTable({
  overview,
  status,
  tableSearch,
}: {
  overview: CoverageOverview;
  status: StatusFilter;
  tableSearch: ReturnType<typeof useTableSearch>;
}) {
  const { t } = useTranslation();
  const language = useLanguage();
  const navigate = useNavigate();

  // The filter chips count what they would show, so the table says how much
  // work sits behind each one before it is clicked.
  const counts = useMemo(() => {
    const inScope = overview.projects.filter((p) => !p.skipped && !p.exclude_reason);
    return {
      all: overview.projects.length,
      yes: inScope.filter((p) => p.applied === "yes").length,
      partial: inScope.filter((p) => p.applied === "partial").length,
      no: inScope.filter((p) => p.applied === "no").length,
      error: inScope.filter((p) => p.applied === "error").length,
      skipped: overview.projects.filter((p) => p.skipped).length,
      excluded: overview.projects.filter((p) => !!p.exclude_reason).length + overview.excluded_projects.length,
    };
  }, [overview]);

  const rows = useMemo(() => {
    // Excluded is the one filter that has to reach past the scanned list: a
    // project excluded before it was ever scanned has no verdict row at all.
    const base: CoverageProject[] =
      status === "excluded"
        ? [
            ...overview.projects.filter((p) => !!p.exclude_reason),
            ...overview.excluded_projects.map(
              (p): CoverageProject => ({
                ...p,
                applied: "no",
                branch: "",
                on_default: null,
                format: "",
                ci_wired: false,
                registry_pinned: false,
                muted_scopes: [],
                evidence: [],
                note: "",
                skipped: false,
                exclude_reason: p.reason,
              })
            ),
          ]
        : overview.projects.filter((p) => {
            if (status === "all") return true;
            if (status === "skipped") return p.skipped;
            if (p.skipped || p.exclude_reason) return false;
            return p.applied === status;
          });

    const q = tableSearch.q;
    if (!q) return base;
    if (tableSearch.regex) {
      try {
        const re = new RegExp(q, "i");
        return base.filter((p) => re.test(p.path));
      } catch {
        return base;
      }
    }
    const needle = q.toLowerCase();
    return base.filter((p) => p.path.toLowerCase().includes(needle));
  }, [overview, status, tableSearch.q, tableSearch.regex]);

  // Sorting runs after the filter and the search, so it orders what is on
  // screen rather than the whole scan.
  const { sorted, sort } = useSort<CoverageProject>(rows, {
    path: (p) => p.path,
    // The status column reads as one word, so it sorts by how much work is left
    // rather than by the wire value, which would put "error" first and "yes"
    // last for no reason anybody looking at the column would expect.
    status: (p) => statusRank(p),
    // Wiring shows two halves; sorting it by how many are present puts the
    // fully wired and the untouched at opposite ends.
    wiring: (p) => Number(p.ci_wired) + Number(p.registry_pinned),
    format: (p) => p.format,
    branch: (p) => p.branch || p.default_branch,
    last_activity_at: (p) => p.last_activity_at,
    note: (p) => p.note,
  });

  const CHIP_LABEL: Record<StatusFilter, MessageKey> = {
    all: "coverage.filter-all",
    yes: "coverage.applied",
    partial: "coverage.partial",
    no: "coverage.not-applied",
    error: "coverage.errored",
    skipped: "coverage.no-ci",
    excluded: "coverage.excluded",
  };

  return (
    <section>
      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap gap-1.5">
          {/* Links, not buttons: each filter is an address, so it can be copied
              out and the back button steps through the ones just visited. */}
          {STATUS_FILTERS.map((key) => (
            <Link
              key={key}
              to="/workspace/coverage"
              search={key === "all" ? {} : { status: key }}
              replace
              className={cn(
                "rounded-full border px-3 py-1 text-xs transition-colors hover:no-underline",
                status === key
                  ? "border-transparent bg-[var(--fx-surface-selected)] text-foreground"
                  : "border-[var(--fx-border-subtle)] text-muted-foreground hover:bg-[var(--fx-surface-hover)] hover:text-foreground"
              )}
            >
              {/* opacity-70 put this count at 4.21:1 on an unselected chip and
                  3.85:1 on hover. 80% is the lowest step that clears AA in every
                  chip state in both themes, and it keeps the count subordinate
                  to its label. */}
              {t(CHIP_LABEL[key])} <span className="tabular-nums opacity-80">{counts[key]}</span>
            </Link>
          ))}
        </div>
        <TableSearchControls search={tableSearch} />
      </div>

      <TableWrap>
        <Table>
          <TableHeader>
            <TableRow>
              <SortableHead k="path" sort={sort}>{t("coverage.project")}</SortableHead>
              <SortableHead k="status" sort={sort}>{t("coverage.status")}</SortableHead>
              <SortableHead k="wiring" sort={sort}>{t("coverage.wiring")}</SortableHead>
              <SortableHead k="format" sort={sort}>{t("coverage.format")}</SortableHead>
              <SortableHead k="branch" sort={sort}>{t("coverage.branch")}</SortableHead>
              <SortableHead k="last_activity_at" sort={sort}>{t("coverage.last-activity")}</SortableHead>
              <SortableHead k="note" sort={sort}>{t("coverage.note")}</SortableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {sorted.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="text-center text-muted-foreground">
                  {t("coverage.no-projects")}
                </TableCell>
              </TableRow>
            )}
            {sorted.map((p) => (
              // The whole row navigates, because the row is what a reader is
              // pointing at. The link in the first cell stays: it is what makes
              // the row reachable by keyboard and openable in a new tab, which a
              // click handler alone cannot do.
              <TableRow
                key={p.path}
                className="cursor-pointer"
                onClick={(event) => {
                  if (shouldIgnoreRowClick(event)) return;
                  navigate({ to: "/workspace/coverage/project", search: { path: p.path } });
                }}
              >
                <TableCell className="font-medium">
                  <CopyOnHover value={p.path}>
                    <Link
                      to="/workspace/coverage/project"
                      search={{ path: p.path }}
                      className="min-w-0 break-all hover:underline"
                    >
                      {highlightMatches(p.path, tableSearch.highlightRe)}
                    </Link>
                  </CopyOnHover>
                </TableCell>
                <TableCell>
                  {p.exclude_reason ? (
                    // Muted either way, by an operator or by the repository's own
                    // topic. The icon says it is silenced; the label says who did it.
                    <span className="inline-flex min-w-0 items-center gap-1.5 whitespace-nowrap text-muted-foreground">
                      <VolumeX className="size-3.5 shrink-0" aria-hidden="true" />
                      <span className="min-w-0 truncate">{exclusionLabel(p.exclude_reason, t)}</span>
                    </span>
                  ) : p.skipped ? (
                    <StatusLed tone="skipped">
                      <span className="text-muted-foreground">{t("coverage.no-ci")}</span>
                    </StatusLed>
                  ) : (
                    <StatusLed tone={p.applied}>
                      {t(appliedLabelKey[p.applied] ?? "coverage.applied-no")}
                    </StatusLed>
                  )}
                </TableCell>
                <TableCell className="text-xs text-muted-foreground">
                  {p.skipped || p.exclude_reason ? (
                    "-"
                  ) : (
                    <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-0.5">
                      <WiringMark
                        on={p.ci_wired}
                        label={t("coverage.ci")}
                        muted={isMuted(p.muted_scopes, "ci")}
                      />
                      <WiringMark
                        on={p.registry_pinned}
                        label={t("coverage.registry")}
                        muted={isMuted(p.muted_scopes, "registry")}
                      />
                    </span>
                  )}
                </TableCell>
                <TableCell className="text-muted-foreground">{p.format || "-"}</TableCell>
                <TableCell className="text-muted-foreground">
                  {p.branch || "-"}
                  {p.on_default === false && (
                    <Badge variant="outline" className="ml-1.5">
                      {t("coverage.not-default")}
                    </Badge>
                  )}
                </TableCell>
                {/* Relative, with the exact instant on hover: the column is
                    scanned for "which of these is stale", not read as a date. */}
                <TableCell
                  className="whitespace-nowrap text-muted-foreground"
                  title={formatAbsolute(p.last_activity_at, language) || undefined}
                >
                  {formatRelative(p.last_activity_at)}
                </TableCell>
                <TableCell className="max-w-[280px] truncate text-xs text-muted-foreground" title={p.note}>
                  {p.note || "-"}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </TableWrap>
    </section>
  );
}
