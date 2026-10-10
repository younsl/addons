import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Search } from "lucide-react";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { ReactNode } from "react";
import type { Repository } from "@/services/v1/openapi-types";
import { Alert } from "@/components/app-ui/alert";
import { SEV_COLOR } from "@/components/app-ui/severity-bar";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatFileSize } from "@/utils/format-file-size";
import type { ArtifactFilter } from "@/routes/workspace/repositories/-utils/artifact-filter";

function formatStatTime(d: Date, language: string): string {
  return new Intl.DateTimeFormat(language, {
    year: "numeric", month: "short", day: "2-digit",
    hour: "2-digit", minute: "2-digit", second: "2-digit",
    hour12: false, timeZoneName: "short",
  }).format(d);
}

// Severity order (worst first) for the repo-wide breakdown.
const SEV_ORDER = ["critical", "high", "medium", "low"] as const;

// SeverityBreakdown shows the repo-wide advisory histogram without any hover:
// a segmented bar (width ∝ count, coloured by severity) plus an always-visible
// per-severity count row. Zero advisories renders a clean line.
function SeverityBreakdown({ counts }: { counts: Record<string, number> }) {
  const { t } = useTranslation();
  const total = SEV_ORDER.reduce((n, s) => n + (counts[s] ?? 0), 0);
  if (total === 0) {
    return <p className="m-0 text-sm text-[var(--fx-success)]">{t("repo.stat-all-clean")}</p>;
  }
  return (
    <div>
      <div className="flex h-3 w-full gap-px overflow-hidden rounded-[5px] bg-border">
        {SEV_ORDER.map((s) => (counts[s] ?? 0) > 0 && (
          <span key={s} className="h-full min-w-[3px]" style={{ flexGrow: counts[s], backgroundColor: SEV_COLOR[s] }} title={`${s} ${counts[s]}`} />
        ))}
      </div>
      <div className="mt-3 grid grid-cols-2 gap-x-4 gap-y-1.5 text-xs sm:grid-cols-4">
        {SEV_ORDER.map((s) => (
          <span key={s} className="inline-flex items-center gap-1.5">
            <span className="inline-block size-2.5 shrink-0 rounded-[3px]" style={{ backgroundColor: SEV_COLOR[s] }} />
            <span className="capitalize text-muted-foreground">{s}</span>
            <span className="ml-auto font-medium tabular-nums">{counts[s] ?? 0}</span>
          </span>
        ))}
      </div>
      <div className="mt-3 border-t border-border pt-2 text-xs text-muted-foreground tabular-nums">
        {total} {t("repo.stat-total-vulns")}
      </div>
    </div>
  );
}

// Statistics is an ungated tab (every authenticated reader): it aggregates the
// same artifact listing the Artifacts tab uses into repo-wide counts — stored
// artifacts, size, how many are scanned/clean/vulnerable, distinct licenses,
// labeling coverage, and a repo-wide severity bar. No extra endpoint. Scan
// aggregates are derived
// client-side from per-artifact vuln data, so they cover at most the 500
// most recently accessed artifacts (the listing API's maximum page).
export function Statistics({ repo }: { repo: Repository }) {
  const { t, language } = useTranslation();
  // A sample rather than the whole repository: 500 artifacts is enough for the
  // severity and score distributions to be representative, and the endpoint
  // pages beyond that.
  const artifactsQuery = useQuery({
    ...openApiQueryOptions.listRepositoryArtifacts({ path: { id: repo.id }, query: { limit: 500 } }),
    meta: { suppressGlobalErrorToast: true },
  });
  const data = artifactsQuery.data;
  const error = getErrorMessageIfAny(artifactsQuery.error);
  // When the numbers were last true, taken from the cache rather than stamped
  // by hand - a failed refresh then leaves it at the last good read.
  const updatedAt = artifactsQuery.dataUpdatedAt ? new Date(artifactsQuery.dataUpdatedAt) : null;

  if (error) return <Alert>{error}</Alert>;
  if (!data) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;

  const arts = data.artifacts;
  // Artifacts the server has observed to be unservable: metadata points at blob
  // bytes that are not in the blob store.
  const broken = arts.filter((a) => a.blob_missing);
  // Scanned = a stored scan exists (max_severity populated); clean = "none".
  const scanned = arts.filter((a) => a.max_severity !== undefined);
  const clean = scanned.filter((a) => a.max_severity === "none").length;
  const vulnerable = scanned.length - clean;
  const cleanPct = scanned.length ? Math.round((clean / scanned.length) * 100) : null;
  // Repo-wide severity histogram (sum per-level advisory counts).
  const counts: Record<string, number> = {};
  for (const a of arts) for (const [s, n] of Object.entries(a.vuln_counts ?? {})) counts[s] = (counts[s] ?? 0) + n;
  const licenses = new Set<string>();
  for (const a of arts) (a.licenses ?? []).forEach((l) => licenses.add(l));
  // Unique CVSS scores repo-wide, deduped by advisory id so a coordinate shared
  // across several stored files (jar + pom + sources) is not counted repeatedly.
  const scoreById = new Map<string, number>();
  for (const a of arts) for (const adv of a.vuln_advisories ?? []) {
    const s = parseFloat(adv.score ?? "");
    if (!Number.isNaN(s)) scoreById.set(adv.id, s);
  }
  const scores = [...scoreById.values()];

  // Labeling coverage is counted server-side over the whole repository, not
  // over the 500-artifact sample the scan panels use.
  const labeledPct = data.count ? Math.round((data.labeled_count / data.count) * 100) : null;

  // Status tokens rather than Tailwind's palette: emerald-600 measured 2.64:1 on
  // a light panel.
  const cleanTone = cleanPct === null ? undefined
    : cleanPct === 100 ? "text-[var(--fx-success)]"
      : cleanPct >= 50 ? "text-[var(--fx-warning)]"
        : "text-destructive";

  return (
    <div className="mb-4 flex flex-col gap-3">
      {/* Last aggregation time (client-side, when the listing was fetched), with
          the viewer's timezone, top-right like a Grafana dashboard clock. */}
      <div className="flex items-center justify-end">
        {updatedAt && (
          <span className="text-xs text-muted-foreground tabular-nums">
            {t("repo.stat-updated")}: {formatStatTime(updatedAt, language)}
          </span>
        )}
      </div>
      {/* Grafana-style dashboard: each metric is its own titled panel. */}
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Panel title={t("common.artifacts")} drill={{ repoId: repo.id }}><BigStat value={data.count.toLocaleString()} /></Panel>
        <Panel title={t("common.size")} drill={{ repoId: repo.id }}><BigStat value={formatFileSize(data.total_size)} /></Panel>
        <Panel title={t("repo.stat-scanned")} drill={{ repoId: repo.id, filter: "scanned" }}>
          <BigStat value={scanned.length.toLocaleString()} sub={`/ ${data.count.toLocaleString()}`} />
        </Panel>
        <Panel title={t("repo.clean-ratio")} drill={{ repoId: repo.id, filter: "clean" }}>
          <BigStat value={cleanPct === null ? "-" : `${cleanPct}%`} tone={cleanTone}
            sub={cleanPct === null ? undefined : `${clean.toLocaleString()} / ${scanned.length.toLocaleString()}`} />
        </Panel>
        <Panel title={t("repo.stat-vulnerable")} drill={{ repoId: repo.id, filter: "vulnerable" }}>
          <BigStat value={vulnerable.toLocaleString()} tone={vulnerable > 0 ? "text-destructive" : undefined} />
        </Panel>
        <Panel title={t("repo.stat-licenses")} drill={{ repoId: repo.id, filter: "licensed" }}><BigStat value={licenses.size.toLocaleString()} /></Panel>
        <Panel title={t("repo.stat-labeled")} drill={{ repoId: repo.id, filter: "labeled" }}>
          <BigStat value={labeledPct === null ? "-" : `${labeledPct}%`}
            sub={labeledPct === null ? undefined : `${data.labeled_count.toLocaleString()} / ${data.count.toLocaleString()}`} />
        </Panel>
        <Panel title={t("repo.stat-broken")} drill={{ repoId: repo.id, filter: "broken" }}>
          <BigStat
            value={broken.length.toLocaleString()}
            tone={broken.length > 0 ? "text-destructive" : undefined}
            sub={broken.length > 0 ? `/ ${data.count.toLocaleString()}` : undefined}
          />
        </Panel>
      </div>
      <div className="grid gap-3 lg:grid-cols-2">
        <Panel title={t("repo.stat-vuln-title")} drill={{ repoId: repo.id, filter: "vulnerable" }}>
          {scanned.length === 0
            ? <p className="m-0 text-sm text-muted-foreground">{t("repo.stat-none-scanned")}</p>
            : <SeverityBreakdown counts={counts} />}
        </Panel>
        <Panel title={t("repo.stat-score-title")} drill={{ repoId: repo.id, filter: "vulnerable" }}>
          <ScoreDistribution scores={scores} />
        </Panel>
      </div>
    </div>
  );
}

// Panel is one Grafana-style dashboard tile: a titled, bordered box. The header
// is a small uppercase caption kept to one line, so every tile in a row has the
// same header height. A caption too long for a narrow tile is cut and shown in
// full on hover. `drill` adds a magnifier that opens the Artifacts tab narrowed
// to what the panel counts (no filter for the repository-wide totals).
function Panel({ title, drill, children }: {
  title: string;
  drill?: { repoId: number; filter?: ArtifactFilter };
  children: ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex min-w-0 flex-col rounded-[var(--radius)] border border-border bg-card">
      <div className="flex items-center gap-2 border-b border-border px-3 py-1.5">
        <span className="min-w-0 flex-1 truncate text-[11px] font-medium uppercase tracking-wide text-muted-foreground" title={title}>{title}</span>
        {drill && (
          <Link
            to="/workspace/repositories/$id/$tab"
            params={{ id: String(drill.repoId), tab: "artifacts" }}
            search={drill.filter ? { filter: drill.filter } : {}}
            aria-label={`${t("repo.stat-open-artifacts")}: ${title}`}
            title={t("repo.stat-open-artifacts")}
            className="-my-1 -mr-1.5 inline-flex size-5 shrink-0 items-center justify-center rounded-sm text-muted-foreground transition-colors hover:bg-[var(--fx-surface-hover)] hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
          >
            <Search className="size-3.5" aria-hidden="true" />
          </Link>
        )}
      </div>
      <div className="flex flex-1 flex-col p-3">{children}</div>
    </div>
  );
}

// BigStat is the hero-number body of a stat panel: a large value in an ink/status
// token (never a series colour) with an optional muted sub-line. The sub-line's
// row is reserved even when empty, so values sit on the same line across a row
// whether or not their panel has one.
function BigStat({ value, sub, tone }: { value: string; sub?: string; tone?: string }) {
  return (
    <div className="flex flex-1 flex-col">
      <div className={cn("text-3xl font-semibold leading-none tabular-nums", tone)}>{value}</div>
      <div className="mt-1.5 min-h-4 text-xs leading-4 text-muted-foreground tabular-nums" aria-hidden={sub ? undefined : true}>{sub}</div>
    </div>
  );
}

// CVSS score band boundaries: <4 low, <7 medium, <9 high, else critical. Each
// band's fraction of the 0–10 axis (40/30/20/10%) sets its backdrop width.
const CVSS_BANDS: { key: string; width: number }[] = [
  { key: "low", width: 40 }, { key: "medium", width: 30 },
  { key: "high", width: 20 }, { key: "critical", width: 10 },
];
const scoreBand = (s: number) => (s >= 9 ? "critical" : s >= 7 ? "high" : s >= 4 ? "medium" : "low");

// ScoreDistribution plots unique CVSS scores on a 0–10 axis: the standard
// severity bands as a faint backdrop, the observed min–max as a highlighted
// span, a marker per score, and a per-band count line below.
function ScoreDistribution({ scores }: { scores: number[] }) {
  const { t } = useTranslation();
  if (scores.length === 0) return <p className="m-0 text-sm text-muted-foreground">{t("repo.stat-no-scores")}</p>;
  const min = Math.min(...scores);
  const max = Math.max(...scores);
  const avg = scores.reduce((a, b) => a + b, 0) / scores.length;
  const pct = (v: number) => `${Math.max(0, Math.min(100, (v / 10) * 100))}%`;
  const counts: Record<string, number> = {};
  for (const s of scores) counts[scoreBand(s)] = (counts[scoreBand(s)] ?? 0) + 1;
  const summary: { label: string; value: number }[] = [
    { label: "min", value: min }, { label: "avg", value: avg }, { label: "max", value: max },
  ];
  return (
    <div>
      {/* Emphasised min / avg / max, each coloured by its CVSS band. */}
      <div className="mb-3 flex flex-wrap gap-x-8 gap-y-2">
        {summary.map((s) => (
          <div key={s.label} className="min-w-[3.5rem]">
            <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{s.label}</div>
            <div className="text-3xl font-semibold leading-none tabular-nums" style={{ color: SEV_COLOR[scoreBand(s.value)] }}>{s.value.toFixed(1)}</div>
          </div>
        ))}
      </div>
      <div className="relative h-3 w-full overflow-hidden rounded-[5px] bg-border">
        <div className="absolute inset-0 flex">
          {CVSS_BANDS.map((b) => (
            <span key={b.key} className="h-full opacity-20" style={{ width: `${b.width}%`, backgroundColor: SEV_COLOR[b.key] }} />
          ))}
        </div>
        {/* Observed min–max span. */}
        <span className="absolute top-0 h-full bg-foreground/15"
          style={{ left: pct(min), width: `calc(${pct(max)} - ${pct(min)})` }} />
        {/* One marker per score, coloured by its band. */}
        {scores.map((s, i) => (
          <span key={i} className="absolute top-1/2 size-1.5 -translate-x-1/2 -translate-y-1/2 rounded-full ring-1 ring-card"
            style={{ left: pct(s), backgroundColor: SEV_COLOR[scoreBand(s)] }} title={s.toFixed(1)} />
        ))}
      </div>
      <div className="mt-2.5 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground tabular-nums">
        <span>{scores.length} {t("repo.stat-scored")}</span>
        {CVSS_BANDS.map((b) => (counts[b.key] ?? 0) > 0 && (
          <span key={b.key} className="inline-flex items-center gap-1">
            <span className="inline-block size-2 rounded-[2px]" style={{ backgroundColor: SEV_COLOR[b.key] }} />
            {b.key} {counts[b.key]}
          </span>
        ))}
      </div>
    </div>
  );
}

// Severity rank for sorting the artifact vuln column: worse sorts higher;
// unscanned (undefined) always sinks to the bottom via the empty-value rule.
