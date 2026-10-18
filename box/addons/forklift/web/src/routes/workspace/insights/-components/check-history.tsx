import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { ZoomOut } from "lucide-react";

import { Button } from "@/components/ui/button";

import { useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import type { InsightCheckPoint } from "@/services/v1/openapi-types";
import { ChartHover, TooltipRow, TooltipTime } from "./chart-hover";

type Status = InsightCheckPoint["status"];

const STATUS_COLOR: Record<Status, string> = {
  action: "var(--fx-danger)",
  investigate: "var(--fx-warning)",
  acknowledged: "var(--fx-info)",
  ok: "var(--fx-success)",
  unavailable: "var(--fx-border-subtle)",
};

const DAYS = 30;

function Panel({ title, aside, children }: { title: string; aside?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col rounded-md border border-border p-4">
      <div className="mb-3 flex items-baseline justify-between gap-3">
        <h3 className="m-0 text-xs font-medium uppercase tracking-wide text-muted-foreground">{title}</h3>
        {aside && <span className="text-xs text-muted-foreground">{aside}</span>}
      </div>
      {children}
    </div>
  );
}

const pad = (n: number) => String(n).padStart(2, "0");
const localDate = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const localTime = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;

const localSeconds = (d: Date) => `${localTime(d)}:${pad(d.getSeconds())}`;

// runLabels names each run in local time, as coarse as still tells every run
// from the one before it: minutes, or seconds when two runs share a minute.
// The first run and the first run of each new day carry the month and day.
export function runLabels(times: string[]): string[] {
  const dates = times.map((at) => new Date(at));
  const day = (d: Date, i: number) => (i === 0 || localDate(d) !== localDate(dates[i - 1]) ? `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` : "");
  const minutes = dates.map((d) => `${localDate(d)} ${localTime(d)}`);
  const clock = minutes.every((m, i) => i === 0 || m !== minutes[i - 1]) ? localTime : localSeconds;
  return dates.map((d, i) => `${day(d, i)}${clock(d)}`);
}

export type Stretch = { status: Status; from: string; label: string; runs: number };

// The order a bucket of runs takes its colour from: the most urgent status in
// it wins, so a single action-recommended run is never averaged away.
const URGENCY: Status[] = ["action", "investigate", "acknowledged", "ok", "unavailable"];

// stretches folds consecutive runs with the same status into one stretch,
// labelled with the run it began at. Past `maxBuckets` runs, the runs are first
// grouped into that many equal buckets, each taking its most urgent status, so
// thousands of runs draw as one band per few pixels instead of sub-pixel slivers.
export function stretches(points: InsightCheckPoint[], labels: string[], maxBuckets = Infinity): Stretch[] {
  const n = points.length;
  const buckets = Math.max(1, Math.min(n, Math.floor(maxBuckets)));
  const out: Stretch[] = [];
  for (let b = 0; b < buckets; b++) {
    const start = Math.floor((b * n) / buckets);
    const end = Math.floor(((b + 1) * n) / buckets);
    if (end <= start) continue;
    let status = points[start].status;
    for (let i = start + 1; i < end; i++) {
      if (URGENCY.indexOf(points[i].status) < URGENCY.indexOf(status)) status = points[i].status;
    }
    const prev = out[out.length - 1];
    if (prev && prev.status === status) prev.runs += end - start;
    else out.push({ status, from: points[start].generated_at, label: labels[start], runs: end - start });
  }
  return out;
}

// useWidth tracks an element's rendered width, so a chart can decide how many
// per-run marks and labels fit.
function useWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

// Each run owns an equal slot and is drawn at the slot's centre, so the two
// panels place the same scan at the same relative position.
const centre = (i: number, n: number) => `${((i + 0.5) / n) * 100}%`;

// axisTicks picks which runs the time axis names and what it calls them. A
// window over two days or more is named by day, at the first run of each day;
// a shorter one by time (with the date under the first run of each day). From
// the latest backwards, a label is kept only where it clears the one after it,
// so hundreds of runs never pile their labels on top of each other.
export function axisTicks(times: string[], width: number, charPx = 6.5): { index: number; lines: string[]; left: number }[] {
  const n = times.length;
  if (n === 0 || width <= 0) return [];
  const dates = times.map((at) => new Date(at));
  const span = dates[n - 1].getTime() - dates[0].getTime();
  const daily = span >= 2 * 24 * 60 * 60 * 1000 && localDate(dates[0]) !== localDate(dates[n - 1]);
  const day = (d: Date) => `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const lines: string[][] = daily
    ? dates.map((d) => [day(d)])
    : runLabels(times).map((l) => (l.includes(" ") ? [l.split(" ")[1], l.split(" ")[0]] : [l]));
  const candidates = daily
    ? dates.map((_, i) => i).filter((i) => i === 0 || localDate(dates[i]) !== localDate(dates[i - 1]))
    : dates.map((_, i) => i);
  const slot = width / n;
  const out: { index: number; lines: string[]; left: number }[] = [];
  let limit = Infinity;
  for (const i of [...candidates].reverse()) {
    const w = Math.max(...lines[i].map((l) => l.length)) * charPx;
    const left = Math.min(Math.max(0, (i + 0.5) * slot - w / 2), width - w);
    if (left + w + 8 > limit) continue;
    out.unshift({ index: i, lines: lines[i], left });
    limit = left;
  }
  return out;
}

// Axis marks every run under the chart while the marks stay apart, and names
// the runs axisTicks picks.
function Axis({ points, width }: { points: InsightCheckPoint[]; width: number }) {
  const n = points.length;
  const slot = width / n;
  const ticks = axisTicks(points.map((p) => p.generated_at), width);
  const twoLines = ticks.some((tick) => tick.lines.length > 1);
  return (
    <div className="mt-1 text-[11px] tabular-nums text-muted-foreground" aria-hidden="true">
      {slot >= 4 && (
        <div className="relative h-1">
          {points.map((p, i) => (
            <span key={p.generated_at} className="absolute top-0 h-1 w-px bg-[var(--fx-border-strong)]" style={{ left: centre(i, n) }} />
          ))}
        </div>
      )}
      <div className={cn("relative mt-0.5", twoLines ? "h-8" : "h-4")}>
        {ticks.map((tick) => (
          <span key={points[tick.index].generated_at} className="absolute flex flex-col whitespace-nowrap leading-4" style={{ left: tick.left }}>
            {tick.lines.map((line) => <span key={line}>{line}</span>)}
          </span>
        ))}
      </div>
    </div>
  );
}

type Hover = { index: number; panel: "status" | "flagged" } | null;

type HoverProps = { hover: Hover; onHover: (hover: Hover) => void; onZoom: (first: number, last: number) => void };

// RunTooltip is what the crosshair shows for one run, the same in both panels.
function RunTooltip({ point }: { point: InsightCheckPoint }) {
  const { t } = useTranslation();
  return (
    <>
      <TooltipTime at={point.generated_at} />
      <TooltipRow color={STATUS_COLOR[point.status]} label={t("insights.status")} value={t(`insights.status-${point.status}` as MessageKey)} />
      <TooltipRow color="var(--fx-accent-ink)" label={t("insights.history-flagged")} value={point.flagged.toLocaleString()} />
    </>
  );
}

// StatusTimeline draws one band per stretch of runs with the same status, as
// wide as its share of the runs, and names the status and when it began
// inside the band, so the chart needs no legend or change list. A faint rule
// inside a band marks each run within it.
function StatusTimeline({ points, hover, onHover, onZoom }: { points: InsightCheckPoint[] } & HoverProps) {
  const { t } = useTranslation();
  const [ref, width] = useWidth<HTMLDivElement>();
  const n = points.length;
  const labels = runLabels(points.map((p) => p.generated_at));
  // One bucket per two pixels at most, so every band is wide enough to see.
  const bands = stretches(points, labels, width > 0 ? width / 2 : Infinity);
  return (
    <>
      <div ref={ref} className="relative mt-1.5">
        <div className="relative flex h-24 w-full overflow-hidden rounded" role="img"
          aria-label={bands.slice(-50).map((b) => `${b.label} ${t(`insights.status-${b.status}` as MessageKey)}`).join(", ")}>
          {bands.map((b) => (
            <div key={b.from} className="flex min-w-0 shrink-0 flex-col justify-end px-2 py-1.5 text-[var(--fx-canvas)]"
              style={{ width: `${(b.runs / n) * 100}%`, background: STATUS_COLOR[b.status] }}>
              {/* A band too narrow to name stays a plain colour; the crosshair
                  tooltip still names every run in it. */}
              {(b.runs / n) * width >= 88 && (
                <>
                  <span className="truncate text-xs font-medium">{t(`insights.status-${b.status}` as MessageKey)}</span>
                  <span className="truncate text-[11px] tabular-nums opacity-80">{b.label}</span>
                </>
              )}
            </div>
          ))}
          {width / n >= 4 && points.slice(1).map((p, i) => {
            const change = p.status !== points[i].status;
            return (
              <span key={p.generated_at} aria-hidden="true"
                className={cn("absolute inset-y-0 -translate-x-1/2 bg-[var(--fx-canvas)]", change ? "w-0.5" : "w-px opacity-30")}
                style={{ left: `${((i + 1) / n) * 100}%` }} />
            );
          })}
          {hover && (
            <span aria-hidden="true" className="absolute inset-y-0 bg-[var(--fx-text)] opacity-15"
              style={{ left: `${(hover.index / n) * 100}%`, width: `${100 / n}%` }} />
          )}
        </div>
        <ChartHover n={n} layout="slot" index={hover?.index ?? null}
          onIndex={(index) => onHover(index === null ? null : { index, panel: "status" })} onSelect={onZoom}
          tooltip={hover?.panel === "status" && <RunTooltip point={points[hover.index]} />} />
      </div>
      <Axis points={points} width={width} />
    </>
  );
}

// yTicks picks the values the count axis labels: the top and the floor, and
// the midpoint once there is room for a whole number between them.
export function yTicks(max: number): number[] {
  return max >= 2 ? [max, Math.round(max / 2), 0] : [max, 0];
}

// FlaggedTrend charts how many items the check flagged at each run, on a scale
// from zero so a drop to nothing reads as reaching the floor. Every run gets a
// dot while they fit, and the crosshair gives the figures. The dots are HTML
// so they stay round on the stretched chart.
function FlaggedTrend({ points, hover, onHover, onZoom }: { points: InsightCheckPoint[] } & HoverProps) {
  const [ref, measured] = useWidth<HTMLDivElement>();
  const width = 320;
  const height = 96;
  const n = points.length;
  const max = Math.max(1, ...points.map((p) => p.flagged));
  const yOf = (v: number) => height - (v / max) * (height - 4) - 2;
  const xy = points.map((p, i) => [((i + 0.5) / n) * width, yOf(p.flagged)] as const);
  const line = xy.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
  const area = `${xy[0][0].toFixed(1)},${height} ${line} ${xy[n - 1][0].toFixed(1)},${height}`;
  const ticks = yTicks(max);
  const dots = measured / n >= 6;
  return (
    <>
      {/* The tick labels sit at the pixel rows of their grid lines: the chart
          is h-24, 96px, the same as the viewBox height. */}
      <div className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 pt-1.5">
        <div className="relative h-24 text-right text-[11px] tabular-nums text-muted-foreground" aria-hidden="true">
          <span className="invisible">{max.toLocaleString()}</span>
          {ticks.map((v) => (
            <span key={v} className="absolute right-0 -translate-y-1/2 leading-none" style={{ top: yOf(v) }}>{v.toLocaleString()}</span>
          ))}
        </div>
        <div ref={ref} className="relative min-w-0">
          <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" className="block h-24 w-full" role="img"
            aria-label={`${n} runs, latest ${points[n - 1].flagged}, highest ${max}`}>
            {ticks.map((v) => (
              <line key={v} x1="0" y1={v === 0 ? height - 1 : yOf(v)} x2={width} y2={v === 0 ? height - 1 : yOf(v)}
                stroke="var(--fx-border-subtle)" strokeWidth="1" strokeDasharray={v === 0 ? undefined : "3 3"} vectorEffect="non-scaling-stroke" />
            ))}
            {n > 1 && <polygon points={area} fill="var(--fx-accent-ink)" opacity="0.12" />}
            <polyline points={line} fill="none" stroke="var(--fx-accent-ink)" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
          </svg>
          {points.map((p, i) => {
            const active = hover?.index === i;
            if (!dots && !active && n > 1) return null;
            return (
              <span key={p.generated_at} aria-hidden="true"
                className={cn("absolute -translate-x-1/2 -translate-y-1/2 rounded-full bg-[var(--fx-accent-ink)]",
                  active ? "size-2.5 ring-2 ring-[var(--fx-canvas)]" : "size-1.5")}
                style={{ left: centre(i, n), top: yOf(p.flagged) }} />
            );
          })}
          <ChartHover n={n} layout="slot" index={hover?.index ?? null}
            onIndex={(index) => onHover(index === null ? null : { index, panel: "flagged" })} onSelect={onZoom}
            tooltip={hover?.panel === "flagged" && <RunTooltip point={points[hover.index]} />} />
        </div>
        <span />
        <Axis points={points} width={measured} />
      </div>
    </>
  );
}

export type ZoomRange = { from?: string; to?: string };

// CheckHistory is the Metrics section: how the status changed on the left, how
// many items were flagged on the right, over the last 30 days. The panels share
// one crosshair, so pointing at a run in either marks it in both. Dragging
// across either zooms both into the runs it spans, as in Grafana; the range is
// kept in the URL (`from`, `to`) so a zoomed view can be shared, and Reset zoom
// goes back to the whole window.
export function CheckHistory({ checkId, range, onRange }: {
  checkId: string;
  range: ZoomRange;
  onRange: (range: ZoomRange) => void;
}) {
  const { t } = useTranslation();
  const [hover, setHover] = useState<Hover>(null);
  const query = useQuery({
    ...openApiQueryOptions.getInsightCheckHistory({ path: { id: checkId }, query: { days: DAYS } }),
    meta: { suppressGlobalErrorToast: true },
  });
  const all = query.data?.points ?? [];
  if (query.isError) return null;
  const zoomed = Boolean(range.from || range.to);
  const points = zoomed
    ? all.filter((p) => (!range.from || p.generated_at >= range.from) && (!range.to || p.generated_at <= range.to))
    : all;
  const aside = zoomed && points.length > 0
    ? t("insights.history-zoomed")
      .replace("{from}", runLabels([points[0].generated_at, points[points.length - 1].generated_at]).join(" – "))
      .replace("{runs}", String(points.length))
    : t("insights.history-window").replace("{days}", String(DAYS)).replace("{runs}", String(points.length));
  const shown = hover && hover.index < points.length ? hover : null;
  const zoom = (first: number, last: number) => {
    setHover(null);
    onRange({ from: points[first].generated_at, to: points[last].generated_at });
  };
  const empty = <p className="m-0 text-sm text-muted-foreground">{t("insights.history-empty")}</p>;
  return (
    <section className="mb-5">
      <div className="mb-1.5 flex items-center justify-between gap-3">
        <h2 className="m-0 text-xs font-medium uppercase tracking-wide text-muted-foreground">{t("insights.metrics")}</h2>
        {zoomed ? (
          <Button variant="outline" size="sm" onClick={() => onRange({})}>
            <ZoomOut className="size-3.5" aria-hidden="true" />
            {t("insights.history-reset-zoom")}
          </Button>
        ) : (
          all.length > 1 && <span className="text-xs text-muted-foreground">{t("insights.history-drag-hint")}</span>
        )}
      </div>
      <div className="grid gap-3 md:grid-cols-2">
        <Panel title={t("insights.history-status")} aside={aside}>
          {points.length === 0 ? empty : <StatusTimeline points={points} hover={shown} onHover={setHover} onZoom={zoom} />}
        </Panel>
        <Panel title={t("insights.history-flagged")} aside={aside}>
          {points.length === 0 ? empty : <FlaggedTrend points={points} hover={shown} onHover={setHover} onZoom={zoom} />}
        </Panel>
      </div>
    </section>
  );
}
