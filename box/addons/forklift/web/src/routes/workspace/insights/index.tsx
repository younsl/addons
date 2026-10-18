import { useMemo } from "react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Play, RefreshCw, Settings } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Select } from "@/components/app-ui/select";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { refreshInsights } from "@/services/v1/insights/api";
import type { InsightCheck, InsightFinding, InsightReport, InsightSummary } from "@/services/v1/openapi-types";
import { formatAbsolute, formatRelative } from "@/utils/format-relative";
import { CheckRow, StatusIcon } from "./-components/finding-card";
import { InsightsTabs } from "./-components/insights-tabs";
import { ScoreTrend } from "./-components/score-trend";
import { TooltipRow, TooltipTime } from "./-components/chart-hover";

const CATEGORIES = ["supply-chain", "policy", "exposure", "access", "operations"] as const;
const SEVERITIES = ["critical", "high", "medium", "low", "info"] as const;
const STATUSES = ["action", "investigate", "acknowledged", "ok", "unavailable"] as const;

type Category = (typeof CATEGORIES)[number];
type Severity = (typeof SEVERITIES)[number];
type Status = (typeof STATUSES)[number];

type InsightsSearch = {
  category?: Category;
  severity?: Severity;
  status?: Status;
};

function pick<T extends string>(values: readonly T[], value: unknown): T | undefined {
  return values.includes(value as T) ? (value as T) : undefined;
}

export const Route = createFileRoute("/workspace/insights/")({
  // Every choice on the page lives in the URL, so a link reproduces the exact
  // view: which slice. Each check has its own page.
  validateSearch: (search: Record<string, unknown>): InsightsSearch => {
    const out: InsightsSearch = {};
    const category = pick(CATEGORIES, search.category);
    const severity = pick(SEVERITIES, search.severity);
    if (category) out.category = category;
    if (severity) out.severity = severity;
    const status = pick(STATUSES, search.status);
    if (status) out.status = status;
    return out;
  },
  component: InsightsRoute,
});

const GRADE_TONE: Record<string, string> = {
  A: "text-[var(--fx-success)]",
  B: "text-[var(--fx-success)]",
  C: "text-[var(--fx-warning)]",
  D: "text-[var(--fx-danger)]",
  F: "text-[var(--fx-danger)]",
};

function InsightsRoute() {
  const { t } = useTranslation();
  const search = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const queryClient = useQueryClient();

  const query = useQuery({
    ...openApiQueryOptions.getInsights(),
    meta: { suppressGlobalErrorToast: true },
    // Poll only while a run is in flight, so a refresh lands without a reload.
    refetchInterval: (q) => (q.state.data?.running ? 2000 : false),
  });
  const refresh = useMutation({
    mutationFn: () => refreshInsights(),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getInsights() }),
  });

  const data = query.data;
  const report = data?.report ?? null;
  const checks = useMemo(() => filterChecks(report, search), [report, search]);

  const setSearch = (patch: Partial<InsightsSearch>) =>
    navigate({ search: (prev) => ({ ...prev, ...patch }), replace: true });

  const error = getErrorMessageIfAny(query.error);
  const running = Boolean(data?.running) || refresh.isPending;

  return (
    <>
      <InsightsTabs active="security" />
      <PageHeader
        title={t("insights.title")}
        actions={
          <>
            {data?.can_refresh && (
              <Link to="/workspace/insights/settings" className={buttonVariants({ variant: "outline" })}>
                <Settings className="size-4" aria-hidden="true" />
                {t("nav.settings")}
              </Link>
            )}
            {data?.can_refresh && (
              <Button onClick={() => refresh.mutate()} disabled={running}>
                {running
                  ? <RefreshCw className="size-4 animate-spin" aria-hidden="true" />
                  : <Play className="size-4" aria-hidden="true" />}
                {running ? t("insights.analyzing") : t("insights.analyze-now")}
              </Button>
            )}
          </>
        }
      />
      <PageDescription>{t("insights.description")}</PageDescription>

      {error && <Alert className="mb-4">{error}</Alert>}
      {refresh.isError && <Alert className="mb-4">{getErrorMessageIfAny(refresh.error)}</Alert>}
      {data?.scope === "repositories" && (
        <p className="mb-4 rounded-md border border-border bg-muted/20 px-3 py-2 text-sm text-muted-foreground">
          {t("insights.scope-repositories")}
        </p>
      )}

      {data && !report && !error && (
        <Card><CardContent className="text-sm text-muted-foreground">{t("insights.no-report")}</CardContent></Card>
      )}

      {report && (
        <>
          <Overview report={report} history={data?.history ?? []} intervalSeconds={data?.interval_seconds ?? 0}
            scoped={data?.scope === "repositories"}
            activeSeverity={search.severity}
            onSeverity={(severity) => setSearch({ severity: search.severity === severity ? undefined : severity })} />

          <div className="mb-3 mt-6 flex flex-wrap items-center gap-2">
            <Select size="sm" className="w-44" value={search.category ?? ""}
              onChange={(v) => setSearch({ category: pick(CATEGORIES, v) })}
              options={[{ value: "", label: t("insights.category-all") },
                ...CATEGORIES.map((c) => ({ value: c, label: t(`insights.category-${c}` as MessageKey) }))]} />
            <Select size="sm" className="w-40" value={search.severity ?? ""}
              onChange={(v) => setSearch({ severity: pick(SEVERITIES, v) })}
              options={[{ value: "", label: t("insights.severity-all") },
                ...SEVERITIES.map((s) => ({ value: s, label: t(`insights.severity-${s}` as MessageKey) }))]} />
            <Select size="sm" className="w-48" value={search.status ?? ""}
              onChange={(v) => setSearch({ status: pick(STATUSES, v) })}
              options={[{ value: "", label: t("insights.status-all") },
                ...STATUSES.map((st) => ({ value: st, label: t(`insights.status-${st}` as MessageKey) }))]} />
            <span className="ml-auto text-sm text-muted-foreground">
              {t("insights.showing").replace("{count}", String(checks.length)).replace("{total}", String(report.checks.length))}
            </span>
          </div>

          <CheckGroups report={report} checks={checks} locked={data?.scope !== "all"} />
        </>
      )}
    </>
  );
}

const STATUS_ORDER: Record<Status, number> = { action: 0, investigate: 1, acknowledged: 2, ok: 3, unavailable: 4 };
const SEVERITY_ORDER: Record<Severity, number> = { critical: 0, high: 1, medium: 2, low: 3, info: 4 };

type CheckView = { check: InsightCheck; finding?: InsightFinding };

// filterChecks lists every rule, failing ones first, the way Trusted Advisor
// lists its checks: a passing check is shown too, so the reader sees what was
// verified and not only what is wrong.
function filterChecks(report: InsightReport | null, search: InsightsSearch): CheckView[] {
  if (!report) return [];
  const byId = new Map(report.findings.map((f) => [f.id, f]));
  return report.checks
    .map((check) => ({ check, finding: byId.get(check.id) }))
    .filter(({ check, finding }) =>
      (!search.category || check.category === search.category)
      && (!search.status || check.status === search.status)
      && (!search.severity || finding?.severity === search.severity))
    .sort((a, b) =>
      STATUS_ORDER[a.check.status] - STATUS_ORDER[b.check.status]
      || SEVERITY_ORDER[a.finding?.severity ?? "info"] - SEVERITY_ORDER[b.finding?.severity ?? "info"]
      || b.check.flagged - a.check.flagged);
}

function worst(checks: CheckView[], category: Category): number {
  return Math.min(...checks.filter((v) => v.check.category === category).map((v) => STATUS_ORDER[v.check.status]));
}

// CheckGroups renders the checks under their category, each category with what
// it covers and how its checks stand.
function CheckGroups({ report, checks, locked }: { report: InsightReport; checks: CheckView[]; locked: boolean }) {
  const { t } = useTranslation();
  if (checks.length === 0) {
    return (
      <Card><CardContent className="text-sm text-muted-foreground">
        {report.findings.length === 0 ? t("insights.all-clear") : t("insights.no-match")}
      </CardContent></Card>
    );
  }
  return (
    <div className="flex flex-col gap-4">
      {CATEGORIES.filter((c) => checks.some((v) => v.check.category === c))
        // The category with the most urgent check comes first.
        .sort((a, b) => worst(checks, a) - worst(checks, b))
        .map((category) => {
        const rows = checks.filter((v) => v.check.category === category);
        const tally = (st: Status) => rows.filter((v) => v.check.status === st).length;
        const tallyTip = (st: Status, count: number) =>
          t(rows.length === 1 ? "insights.tally-tip-one" : "insights.tally-tip")
            .replace("{status}", t(`insights.status-${st}` as MessageKey))
            .replace("{count}", count.toLocaleString())
            .replace("{total}", rows.length.toLocaleString());
        return (
          <section key={category} aria-labelledby={`insights-${category}`}>
            <div className="mb-2">
              <h2 id={`insights-${category}`} className="m-0 text-base font-semibold">
                {t(`insights.category-${category}` as MessageKey)}
              </h2>
              <div className="mt-0.5 flex items-end justify-between gap-3">
                <p className="m-0 text-sm text-muted-foreground">{t(`insights.category-desc-${category}` as MessageKey)}</p>
                <span className="flex shrink-0 items-center gap-3 text-sm tabular-nums">
                  {(["action", "investigate", "acknowledged", "ok"] as const).filter((st) => st !== "acknowledged" || tally(st) > 0).map((st) => (
                    <Tooltip key={st}>
                      <TooltipTrigger render={<span tabIndex={0} className="inline-flex items-center gap-1" aria-label={tallyTip(st, tally(st))} />}>
                        <StatusIcon status={st} />{tally(st)}
                      </TooltipTrigger>
                      <TooltipContent>{tallyTip(st, tally(st))}</TooltipContent>
                    </Tooltip>
                  ))}
                </span>
              </div>
            </div>
            <Card className="gap-0 py-0">
              {rows.map((v) => <CheckRow key={v.check.id} check={v.check} finding={v.finding} showCategory={false} locked={locked} />)}
            </Card>
          </section>
        );
      })}
    </div>
  );
}

// The bar and legend colour per severity. Info carries no penalty, so it stays
// neutral.
const SEVERITY_COLOR: Record<Severity, string> = {
  critical: "var(--fx-severity-critical)",
  high: "var(--fx-severity-high)",
  medium: "var(--fx-severity-medium)",
  low: "var(--fx-severity-low)",
  info: "var(--fx-border-subtle)",
};

// Overview is one quiet panel in the Vercel manner: the score and its trend,
// the finding mix as a single proportional bar whose legend filters the list,
// and what was analyzed as a line of figures underneath. No card per number.
// With `scoped`, the score, the severity mix and the figures cover only the
// repositories the viewer can read and the viewer's own access tokens, and
// say so.
function Overview({ report, history, intervalSeconds, scoped, activeSeverity, onSeverity }: {
  report: InsightReport;
  scoped: boolean;
  history: InsightSummary[];
  intervalSeconds: number;
  activeSeverity?: Severity;
  onSeverity: (severity: Severity) => void;
}) {
  const { t } = useTranslation();
  const inv = report.inventory;
  const scanPct = inv.scannable ? Math.round((inv.scanned / inv.scannable) * 100) : null;
  const total = SEVERITIES.reduce((n, s) => n + report.counts[s], 0);
  const passed = report.checks.filter((c) => c.status === "ok").length;
  const acknowledged = report.checks.filter((c) => c.status === "acknowledged").length;
  const figures = [
    { label: t(scoped ? "insights.inv-repositories-yours" : "insights.inv-repositories"), value: inv.repositories.toLocaleString() },
    { label: t("insights.inv-artifacts"), value: inv.artifacts.toLocaleString() },
    { label: t("insights.inv-images"), value: inv.oci_images.toLocaleString() },
    { label: t("insights.inv-scanned"), value: scanPct === null ? "-" : `${scanPct}%` },
    { label: t(scoped ? "insights.inv-tokens-yours" : "insights.inv-tokens"), value: inv.tokens.toLocaleString() },
  ];
  return (
    <Card className="gap-0 py-0">
      <div className="grid md:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <div className="flex flex-col gap-3 p-5 md:border-r md:border-border">
          <div className="text-xs text-muted-foreground">{t(scoped ? "insights.score-yours" : "insights.score")}</div>
          <div className="flex items-baseline gap-2">
            <span className="text-4xl font-semibold tabular-nums tracking-tight">{report.score}</span>
            <span className="text-lg tabular-nums text-muted-foreground">/ 100</span>
            <span className={cn("ml-1 rounded border border-current px-1.5 text-xs font-semibold leading-5", GRADE_TONE[report.grade])}>
              {report.grade}
            </span>
          </div>
          <ScoreTrend history={history} tooltip={(run) => (
            <>
              <TooltipTime at={run.generated_at} />
              <TooltipRow color="var(--fx-accent-ink)" label={t("insights.score")} value={`${run.score} / 100 (${run.grade})`} />
              {SEVERITIES.map((s) => (
                <TooltipRow key={s} color={SEVERITY_COLOR[s]} label={t(`insights.severity-${s}` as MessageKey)} value={run.counts[s]} />
              ))}
            </>
          )} />
          <div className="text-xs text-muted-foreground">
            {t("insights.analyzed")} {formatRelative(report.generated_at)} ({formatAbsolute(report.generated_at)})
            {" · "}{report.triggered_by === "schedule" ? t("insights.by-schedule") : report.triggered_by}
            {intervalSeconds > 0 && <> · {t("insights.every").replace("{minutes}", String(Math.round(intervalSeconds / 60)))}</>}
          </div>
        </div>
        <div className="flex flex-col justify-center gap-4 border-t border-border p-5 md:border-t-0">
          <div className="flex items-baseline justify-between gap-3">
            <span className="text-xs text-muted-foreground">{t("insights.findings")}</span>
            <span className="text-xs text-muted-foreground">
              {t("insights.checks-passed").replace("{passed}", String(passed)).replace("{total}", String(report.checks.length))}
              {acknowledged > 0 && <> · {t("insights.checks-acknowledged").replace("{count}", String(acknowledged))}</>}
            </span>
          </div>
          <div className="flex h-2 w-full overflow-hidden rounded-full bg-[var(--fx-control)]" role="img"
            aria-label={SEVERITIES.map((s) => `${t(`insights.severity-${s}` as MessageKey)} ${report.counts[s]}`).join(", ")}>
            {total > 0 && SEVERITIES.filter((s) => report.counts[s] > 0).map((s) => (
              <span key={s} style={{ width: `${(report.counts[s] / total) * 100}%`, background: SEVERITY_COLOR[s] }} />
            ))}
          </div>
          <div className="flex flex-wrap gap-x-5 gap-y-2">
            {SEVERITIES.map((s) => (
              <button key={s} type="button" aria-pressed={activeSeverity === s} onClick={() => onSeverity(s)}
                className={cn("inline-flex items-center gap-2 rounded px-1 -mx-1 text-sm transition-colors",
                  activeSeverity === s ? "bg-[var(--fx-control)] text-foreground" : "text-muted-foreground hover:text-foreground")}>
                <span className="size-2 rounded-full" style={{ background: SEVERITY_COLOR[s] }} aria-hidden="true" />
                {t(`insights.severity-${s}` as MessageKey)}
                <span className="font-medium tabular-nums text-foreground">{report.counts[s]}</span>
              </button>
            ))}
          </div>
        </div>
      </div>
      <dl className="m-0 grid grid-cols-2 border-t border-border sm:grid-cols-5">
        {figures.map((f, i) => (
          <div key={f.label} className={cn("px-5 py-3", i > 0 && "sm:border-l sm:border-border")}>
            <dt className="text-xs text-muted-foreground">{f.label}</dt>
            <dd className="m-0 mt-0.5 text-base font-medium tabular-nums">{f.value}</dd>
          </div>
        ))}
      </dl>
    </Card>
  );
}
