import { useMemo, useState } from "react";
import type { CoverageSnapshot } from "@/services/v1/openapi-types";
import { useLanguage, useTranslation } from "@/lib/i18n";

const VIEW_W = 720;
const VIEW_H = 260;
const PAD = { top: 16, right: 16, bottom: 34, left: 36 };

const INNER_W = VIEW_W - PAD.left - PAD.right;
const INNER_H = VIEW_H - PAD.top - PAD.bottom;

// The two series read as "done" and "started", so they borrow the success and
// warning tones the rest of the console already uses for those states.
const APPLIED_COLOR = "var(--fx-success, #10b981)";
const PARTIAL_COLOR = "var(--fx-warning, #f59e0b)";

// withPartial is coverage counting a partially wired project as progress. It is
// the upper band of the chart: the gap to the applied line is the work that is
// started but not finished.
function withPartial(s: CoverageSnapshot): number {
  return s.target > 0 ? Math.round(((s.applied + s.partial) / s.target) * 100) : 0;
}

// dedupeByDay keeps the last scan of each day. Several scans in one day would
// otherwise stack on the same x position and read as noise rather than trend.
function dedupeByDay(items: CoverageSnapshot[]): CoverageSnapshot[] {
  const byDay = new Map<string, CoverageSnapshot>();
  for (const s of items) byDay.set(s.scanned_at.slice(0, 10), s);
  return Array.from(byDay.values()).sort(
    (a, b) => new Date(a.scanned_at).getTime() - new Date(b.scanned_at).getTime()
  );
}

interface Point {
  x: number;
  y: number;
  snapshot: CoverageSnapshot;
  applied: number;
  partial: number;
}

// CoverageTrendChart plots coverage over the snapshots' real timestamps, so a
// week with no scans shows as a gap on the axis instead of being collapsed into
// evenly spaced points that imply a scan that never happened.
export function CoverageTrendChart({
  snapshots: raw,
  retentionDays,
}: {
  snapshots: CoverageSnapshot[];
  retentionDays: number;
}) {
  const { t } = useTranslation();
  const language = useLanguage();
  const snapshots = useMemo(() => dedupeByDay(raw), [raw]);
  const [hover, setHover] = useState<number | null>(null);

  const { appliedPoints, partialPoints, xTicks } = useMemo(() => {
    if (snapshots.length === 0) {
      return { appliedPoints: [] as Point[], partialPoints: [] as Point[], xTicks: [] as Point[] };
    }
    const times = snapshots.map((s) => new Date(s.scanned_at).getTime());
    const min = Math.min(...times);
    const max = Math.max(...times);
    // A single sample, or several inside one day, would divide by zero.
    const span = max - min || 1;
    const xOf = (time: number) =>
      snapshots.length === 1 ? INNER_W / 2 : ((time - min) / span) * INNER_W;
    const yOf = (percent: number) => INNER_H - (percent / 100) * INNER_H;

    const toPoint = (s: CoverageSnapshot, i: number, percent: number): Point => ({
      x: xOf(times[i]),
      y: yOf(percent),
      snapshot: s,
      applied: s.percent,
      partial: withPartial(s),
    });
    const applied = snapshots.map((s, i) => toPoint(s, i, s.percent));
    const partial = snapshots.map((s, i) => toPoint(s, i, withPartial(s)));

    // Cap the label count so a dense history does not overlap on the axis.
    const step = Math.max(1, Math.ceil(applied.length / 8));
    const ticks = applied.filter((_, i) => i % step === 0 || i === applied.length - 1);
    return { appliedPoints: applied, partialPoints: partial, xTicks: ticks };
  }, [snapshots]);

  if (snapshots.length === 0) {
    return (
      <div className="flex min-h-[160px] flex-col items-center justify-center gap-1 text-sm text-muted-foreground">
        <span>{t("coverage.trend-empty")}</span>
        <span className="text-xs text-[var(--fx-text-subtle)]">
          {t("coverage.retention-note")} {retentionDays}
          {t("coverage.days-suffix")}
        </span>
      </div>
    );
  }

  const line = (points: Point[]) =>
    points.map((p, i) => `${i === 0 ? "M" : "L"}${p.x},${p.y}`).join(" ");
  const area = (points: Point[]) =>
    `${line(points)} L${points[points.length - 1].x},${INNER_H} L${points[0].x},${INNER_H} Z`;

  const active = hover !== null ? appliedPoints[hover] : null;
  const formatDay = (iso: string) => {
    const d = new Date(iso);
    return `${d.getMonth() + 1}/${d.getDate()}`;
  };

  return (
    <div className="flex flex-col gap-2">
      <svg
        viewBox={`0 0 ${VIEW_W} ${VIEW_H}`}
        width="100%"
        height={VIEW_H}
        className="block max-w-full"
        role="img"
        aria-label={t("coverage.trend-title")}
      >
        <g transform={`translate(${PAD.left},${PAD.top})`}>
          {[0, 25, 50, 75, 100].map((tick) => {
            const y = INNER_H - (tick / 100) * INNER_H;
            return (
              <g key={tick}>
                <line
                  x1={0}
                  y1={y}
                  x2={INNER_W}
                  y2={y}
                  stroke="var(--fx-border-subtle)"
                  strokeDasharray="3,3"
                />
                <text x={-8} y={y + 4} textAnchor="end" fontSize={10} className="fill-muted-foreground">
                  {tick}%
                </text>
              </g>
            );
          })}

          {appliedPoints.length > 1 && (
            <>
              <path d={area(appliedPoints)} fill={APPLIED_COLOR} opacity={0.12} />
              <path
                d={line(partialPoints)}
                fill="none"
                stroke={PARTIAL_COLOR}
                strokeWidth={1.5}
                strokeDasharray="4,3"
              />
              <path d={line(appliedPoints)} fill="none" stroke={APPLIED_COLOR} strokeWidth={2} />
            </>
          )}

          {appliedPoints.map((p, i) => (
            <circle key={`p-${p.snapshot.scanned_at}`} cx={p.x} cy={p.y} r={hover === i ? 4 : 2.5} fill={APPLIED_COLOR} />
          ))}

          {active && (
            <line x1={active.x} y1={0} x2={active.x} y2={INNER_H} stroke="var(--fx-border-subtle)" />
          )}

          {/* One invisible hit area per point, so the readout follows the pointer
              without every circle needing to be hit exactly. */}
          {appliedPoints.map((p, i) => {
            const half = appliedPoints.length > 1 ? INNER_W / appliedPoints.length / 2 : INNER_W / 2;
            return (
              <rect
                key={`hit-${p.snapshot.scanned_at}`}
                x={p.x - half}
                y={0}
                width={half * 2}
                height={INNER_H}
                fill="transparent"
                onMouseEnter={() => setHover(i)}
                onMouseLeave={() => setHover(null)}
              />
            );
          })}

          {xTicks.map((p) => (
            <text
              key={`x-${p.snapshot.scanned_at}`}
              x={p.x}
              y={INNER_H + 18}
              textAnchor="middle"
              fontSize={10}
              className="fill-muted-foreground"
            >
              {formatDay(p.snapshot.scanned_at)}
            </text>
          ))}
        </g>
      </svg>

      <div className="flex flex-wrap items-center justify-center gap-3 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <svg width={14} height={4} aria-hidden="true">
            <rect width={14} height={3} rx={1.5} fill={APPLIED_COLOR} />
          </svg>
          {t("coverage.applied")}
        </span>
        <span className="inline-flex items-center gap-1.5">
          <svg width={14} height={4} aria-hidden="true">
            <rect width={5} height={3} rx={1.5} fill={PARTIAL_COLOR} />
            <rect x={8} width={6} height={3} rx={1.5} fill={PARTIAL_COLOR} />
          </svg>
          {t("coverage.applied-with-partial")}
        </span>
        <span>
          {active
            ? `${new Date(active.snapshot.scanned_at).toLocaleDateString(language === "ko" ? "ko-KR" : "en-US")}, ${active.applied}% ${t("coverage.applied").toLowerCase()}, ${active.partial}% ${t("coverage.with-partial")}, ${active.snapshot.target} ${t("coverage.projects-word")}`
            : `${snapshots.length} ${t("coverage.scans-word")}`}
        </span>
      </div>
      {/* The retention is stated, not inferred: a line that stops short can mean
          a short history or a scan that stopped running, and those are not the
          same thing to look into. */}
      <p className="text-right text-xs text-[var(--fx-text-subtle)]">
        {t("coverage.retention-note")} {retentionDays}
        {t("coverage.days-suffix")}
      </p>
    </div>
  );
}
