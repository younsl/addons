import { useState, type ReactNode } from "react";

import type { InsightSummary } from "@/services/v1/openapi-types";
import { ChartHover, fractionOf } from "./chart-hover";

const TICKS = [100, 50, 0];

// ScoreTrend draws the score of each run as a line on a fixed 0-100 scale, so
// a flat line at the top reads as good and a drop is visible at a glance.
// Pointing at the chart gives the run's figures through `tooltip`. The tick
// labels sit at the pixel rows of their grid lines: the chart is h-16, 64px,
// the same as the viewBox height.
export function ScoreTrend({ history, tooltip }: { history: InsightSummary[]; tooltip: (run: InsightSummary) => ReactNode }) {
  const [index, setIndex] = useState<number | null>(null);
  if (history.length < 2) return null;
  const width = 240;
  const height = 64;
  const n = history.length;
  const yOf = (score: number) => height - 2 - (score / 100) * (height - 4);
  const step = width / (n - 1);
  const points = history.map((h, i) => `${(i * step).toFixed(1)},${yOf(h.score).toFixed(1)}`).join(" ");
  const active = index !== null && index < n ? index : null;
  return (
    <div className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 py-1.5">
      <div className="relative h-16 text-right text-[11px] tabular-nums text-muted-foreground" aria-hidden="true">
        <span className="invisible">100</span>
        {TICKS.map((v) => (
          <span key={v} className="absolute right-0 -translate-y-1/2 leading-none" style={{ top: yOf(v) }}>{v}</span>
        ))}
      </div>
      <div className="relative min-w-0">
        <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" className="block h-16 w-full" aria-hidden="true">
          {TICKS.map((v) => (
            <line key={v} x1="0" y1={yOf(v)} x2={width} y2={yOf(v)} stroke="var(--fx-border-subtle)" strokeWidth="1"
              strokeDasharray={v === 0 ? undefined : "3 3"} vectorEffect="non-scaling-stroke" />
          ))}
          <polyline points={points} fill="none" stroke="var(--fx-accent-ink)" strokeWidth="1.5"
            vectorEffect="non-scaling-stroke" />
        </svg>
        {active !== null && (
          <span aria-hidden="true" className="absolute size-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-[var(--fx-accent-ink)] ring-2 ring-[var(--fx-canvas)]"
            style={{ left: `${fractionOf(active, n, "edge") * 100}%`, top: yOf(history[active].score) }} />
        )}
        <ChartHover n={n} layout="edge" index={active} onIndex={setIndex}
          tooltip={active !== null && tooltip(history[active])} />
      </div>
    </div>
  );
}
