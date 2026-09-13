import { type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertCircle, Check, ExternalLink, Play, RefreshCw, VolumeX, X } from "lucide-react";
import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
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
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { openApiQueryKeys } from "@/query/v1/openapi-query-keys";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { postScanCoverage } from "@/services/v1/coverage/api";
import type { CoverageGroup, CoverageOverview } from "@/services/v1/openapi-types";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

// While a scan runs the picture changes every few seconds, so the page polls;
// once it settles there is nothing to poll for until the next scan.
const SCANNING_REFRESH_MS = 3_000;
const IDLE_REFRESH_MS = 60_000;

// The two views of the same scan. They are separate routes rather than a
// client-side toggle so each one can be linked to: the alarm points at the
// project list, and a group owner can be sent straight to the breakdown.
export type CoverageView = "list" | "groups";

// useCoverageOverview owns the shared query and the manual scan, so both views
// read one cache entry instead of each fetching the overview for itself.
export function useCoverageOverview() {
  const queryClient = useQueryClient();
  const query = useQuery({
    ...openApiQueryOptions.getCoverage(),
    refetchInterval: (q) =>
      (q.state.data as CoverageOverview | undefined)?.scanning ? SCANNING_REFRESH_MS : IDLE_REFRESH_MS,
  });
  // The connection is checked here rather than on the settings form: it is
  // deployment configuration with nothing to edit, and the only moment it is
  // worth a reader's attention is when it is broken. Before the first scan it is
  // also the only thing that can explain an empty page, since last_scan_error
  // needs a scan to have failed first.
  const gitlab = useQuery({
    ...openApiQueryOptions.getCoverageGitlabCheck(),
    retry: false,
    staleTime: 60_000,
  });
  const scan = useMutation({
    mutationFn: () => postScanCoverage(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverage() });
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listCoverageGroups() });
    },
  });
  return { ...query, scan, gitlab: gitlab.data };
}

// CoverageShell is everything both views share: the header and its actions, the
// state of the last scan, the headline counts, and the tabs between them. Each
// view renders only what is its own below it.
export function CoverageShell({
  overview,
  scan,
  gitlab,
  active,
  children,
}: {
  overview: CoverageOverview;
  scan: ReturnType<typeof useCoverageOverview>["scan"];
  gitlab: ReturnType<typeof useCoverageOverview>["gitlab"];
  active: CoverageView;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const { me } = useAuth();

  return (
    <>
      <PageHeader
        title={t("coverage.title")}
        actions={
          me.admin ? (
            <>
              <Link
                to="/workspace/coverage/settings"
                className={buttonVariants({ variant: "outline" })}
              >
                {t("coverage.settings")}
              </Link>
              <Button
                onClick={() => scan.mutate()}
                disabled={overview.scanning || scan.isPending || !overview.configured}
                title={overview.configured ? undefined : t("coverage.not-configured")}
              >
                {overview.scanning ? (
                  <RefreshCw className="size-4 animate-spin" aria-hidden="true" />
                ) : (
                  <Play className="size-4" aria-hidden="true" />
                )}
                {overview.scanning ? t("coverage.scanning") : t("coverage.scan-now")}
              </Button>
            </>
          ) : null
        }
      />
      <PageDescription>{t("coverage.description")}</PageDescription>

      {!overview.enabled && (
        <Notice>
          {overview.credentials_present ? t("coverage.disabled") : t("coverage.gitlab-missing")}
        </Notice>
      )}
      {overview.enabled && !overview.configured && <Notice>{t("coverage.host-missing")}</Notice>}
      {gitlab?.configured && !gitlab.reachable && gitlab.error && (
        <Alert className="mb-4">
          <AlertCircle className="size-4" aria-hidden="true" />
          {t("coverage.gitlab-unreachable")}: {gitlab.error}
        </Alert>
      )}
      {overview.last_scan_error && (
        <Alert className="mb-4">
          <AlertCircle className="size-4" aria-hidden="true" />
          {t("coverage.last-scan-failed")}: {overview.last_scan_error}
        </Alert>
      )}
      {scan.isError && <Alert className="mb-4">{(scan.error as Error).message}</Alert>}

      <ScanStatus overview={overview} />
      <SummaryCards overview={overview} />
      <ViewTabs active={active} />
      {children}
    </>
  );
}

// ViewTabs links the two views. They are links, not buttons, because each view
// has its own address and the browser's back button should move between them.
function ViewTabs({ active }: { active: CoverageView }) {
  const { t } = useTranslation();
  const tabClass = (on: boolean) =>
    cn(
      "border-b-2 px-1 pb-2 text-sm transition-colors hover:no-underline",
      on
        ? "border-accent-ink text-foreground"
        : "border-transparent text-muted-foreground hover:text-foreground"
    );
  return (
    <nav className="mt-6 mb-4 flex gap-4 border-b border-[var(--fx-border-subtle)]">
      <Link to="/workspace/coverage" className={tabClass(active === "list")}>
        {t("coverage.tab-list")}
      </Link>
      <Link to="/workspace/coverage/groups" className={tabClass(active === "groups")}>
        {t("coverage.tab-groups")}
      </Link>
    </nav>
  );
}

// Notice states something the page needs configured before its numbers mean
// anything. It is not an error: the page still renders, it just has nothing
// measured to show yet.
export function Notice({ children }: { children: ReactNode }) {
  return (
    <div className="mb-4 rounded-md border border-accent-ink/40 bg-primary/10 px-3 py-2 text-sm text-foreground">
      {children}
    </div>
  );
}

// ScanStatus is the one-line answer to "is this number current": when it was
// measured, who asked for it, and, while a scan runs, how far it has got.
function ScanStatus({ overview }: { overview: CoverageOverview }) {
  const { t } = useTranslation();
  const formatDateTime = useDateTime();
  const progress = overview.scan_progress;

  if (overview.scanning && progress) {
    const pct = progress.total > 0 ? Math.round((progress.done / progress.total) * 100) : 0;
    return (
      <div className="mb-4 rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel)] p-3">
        <div className="mb-2 flex items-center justify-between gap-3 text-sm">
          <span className="inline-flex items-center gap-2">
            <RefreshCw className="size-4 animate-spin" aria-hidden="true" />
            {progress.phase === "listing"
              ? t("coverage.progress-listing")
              : `${t("coverage.progress-scanning")} ${progress.done}/${progress.total}`}
          </span>
          <span className="text-muted-foreground">{pct}%</span>
        </div>
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-[var(--fx-surface-hover)]">
          <div className="h-full rounded-full bg-primary transition-[width]" style={{ width: `${pct}%` }} />
        </div>
      </div>
    );
  }

  return (
    <p className="mb-4 text-sm text-muted-foreground">
      {overview.last_scanned_at ? (
        <>
          {t("coverage.last-scanned")}: {formatDateTime(overview.last_scanned_at)}
          {overview.last_scan_triggered_by && ` (${overview.last_scan_triggered_by})`}
          {overview.last_scan_duration_ms > 0 &&
            `, ${Math.round(overview.last_scan_duration_ms / 1000)}s`}
        </>
      ) : (
        t("coverage.never-scanned")
      )}
      {overview.next_run_at && overview.auto_scan_enabled && (
        <>
          {", "}
          {t("coverage.next-run")}: {formatDateTime(overview.next_run_at)}
        </>
      )}
    </p>
  );
}

// SummaryCards leads with the percentage, because that is the number people
// act on; the breakdown behind it follows in the order work moves through it.
function SummaryCards({ overview }: { overview: CoverageOverview }) {
  const { t } = useTranslation();
  const cards: Array<{ label: string; value: string; tone?: string }> = [
    { label: t("coverage.coverage"), value: `${overview.percent}%`, tone: "text-accent-ink" },
    { label: t("coverage.target"), value: String(overview.target) },
    { label: t("coverage.applied"), value: String(overview.applied), tone: "text-[var(--fx-success)]" },
    { label: t("coverage.partial"), value: String(overview.partial), tone: "text-[var(--fx-warning)]" },
    { label: t("coverage.not-applied"), value: String(overview.not_applied), tone: "text-[var(--fx-danger)]" },
    { label: t("coverage.no-ci"), value: String(overview.skipped) },
    { label: t("coverage.excluded"), value: String(overview.excluded) },
  ];
  if (overview.errored > 0) {
    cards.push({ label: t("coverage.errored"), value: String(overview.errored), tone: "text-[var(--fx-danger)]" });
  }
  return (
    <div className="grid grid-cols-2 gap-3 sm:grid-cols-4 lg:grid-cols-7">
      {cards.map((card) => (
        <Card key={card.label}>
          <CardContent className="px-4 py-3">
            <div className="text-xs text-muted-foreground">{card.label}</div>
            <div className={cn("mt-1 text-2xl font-semibold tabular-nums", card.tone)}>{card.value}</div>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

// WiringMark is one half of a project's wiring: the CI pipeline, or the registry
// file that pins where packages resolve from. The icon leads the label because
// colour alone was carrying the state, which is invisible to anyone who cannot
// separate the two greens and hard to scan down a column either way.
//
// A muted half is neither a tick nor a cross: whether it is wired was not asked,
// so it gets the silenced icon and says so in the label rather than showing a
// state that is not being counted.
export function WiringMark({ on, label, muted = false }: { on: boolean; label: string; muted?: boolean }) {
  const { t } = useTranslation();
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 whitespace-nowrap",
        muted ? "text-muted-foreground" : on ? "text-[var(--fx-success)]" : "text-muted-foreground"
      )}
    >
      {muted ? (
        <VolumeX className="size-3.5 shrink-0" aria-hidden="true" />
      ) : on ? (
        <Check className="size-3.5 shrink-0" aria-hidden="true" />
      ) : (
        <X className="size-3.5 shrink-0" aria-hidden="true" />
      )}
      {label}
      {muted && <span className="opacity-70">({t("coverage.muted-check")})</span>}
    </span>
  );
}

// GroupLink opens the group in GitLab, which is where the work of fixing a low
// number actually happens. "(root)" is the placeholder for projects with no
// namespace, so it is not a path and does not link anywhere.
function GroupLink({ group, gitlabURL }: { group: string; gitlabURL: string }) {
  if (!gitlabURL || group === "(root)") return <>{group}</>;
  return (
    <a
      href={`${gitlabURL}/${group}`}
      target="_blank"
      rel="noreferrer"
      className="inline-flex min-w-0 items-center gap-1.5 hover:underline"
      // The row is not clickable here, so the link needs no guard against one.
    >
      <span className="min-w-0 truncate">{group}</span>
      <ExternalLink className="size-3 shrink-0 opacity-60" aria-hidden="true" />
    </a>
  );
}

export function GroupTable({
  groups,
  gitlabURL,
}: {
  groups: CoverageGroup[];
  gitlabURL: string;
}) {
  const { t } = useTranslation();
  // Worst coverage first is the order the server sends and the one this page is
  // read in, so it stays the default; every column is sortable from there.
  const { sorted, sort } = useSort<CoverageGroup>(groups, {
    group: (g) => g.group,
    target: (g) => g.target,
    applied: (g) => g.applied,
    partial: (g) => g.partial,
    not_applied: (g) => g.not_applied,
    percent: (g) => g.percent,
  });

  return (
    <section>
      <TableWrap>
        <Table>
          <TableHeader>
            <TableRow>
              <SortableHead k="group" sort={sort}>{t("coverage.group")}</SortableHead>
              <SortableHead k="target" sort={sort} className="text-right">{t("coverage.target")}</SortableHead>
              <SortableHead k="applied" sort={sort} className="text-right">{t("coverage.applied")}</SortableHead>
              <SortableHead k="partial" sort={sort} className="text-right">{t("coverage.partial")}</SortableHead>
              <SortableHead k="not_applied" sort={sort} className="text-right">{t("coverage.not-applied")}</SortableHead>
              <SortableHead k="percent" sort={sort} className="w-[200px]">{t("coverage.coverage")}</SortableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {sorted.map((g) => (
              <TableRow key={g.group}>
                <TableCell className="font-medium">
                  <GroupLink group={g.group} gitlabURL={gitlabURL} />
                </TableCell>
                <TableCell className="text-right tabular-nums">{g.target}</TableCell>
                <TableCell className="text-right tabular-nums">{g.applied}</TableCell>
                <TableCell className="text-right tabular-nums">{g.partial}</TableCell>
                <TableCell className="text-right tabular-nums">{g.not_applied}</TableCell>
                <TableCell>
                  <div className="flex items-center gap-2">
                    <div className="h-1.5 w-full min-w-[80px] overflow-hidden rounded-full bg-[var(--fx-surface-hover)]">
                      <div
                        className="h-full rounded-full bg-[var(--fx-success)]"
                        style={{ width: `${g.percent}%` }}
                      />
                    </div>
                    <span className="w-10 shrink-0 text-right tabular-nums text-muted-foreground">{g.percent}%</span>
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </TableWrap>
    </section>
  );
}
