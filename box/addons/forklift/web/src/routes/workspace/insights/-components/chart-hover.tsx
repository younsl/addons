import { useState, type ReactNode } from "react";

// "slot": each run owns an equal slot and sits at its centre. "edge": the
// first and last runs sit on the chart's edges.
export type Layout = "slot" | "edge";

export const fractionOf = (i: number, n: number, layout: Layout) =>
  layout === "slot" ? (i + 0.5) / n : n > 1 ? i / (n - 1) : 0.5;

// indexAt is the run nearest to a horizontal position given as a fraction of
// the chart's width.
export function indexAt(f: number, n: number, layout: Layout): number {
  const i = layout === "slot" ? Math.floor(f * n) : Math.round(f * (n - 1));
  return Math.min(n - 1, Math.max(0, i));
}

// ChartHover lays a Grafana style crosshair over a chart: the pointer snaps to
// the nearest run, a vertical rule marks it, and the tooltip, when given,
// opens beside the rule on the side with more room. The rule follows `index`
// even without a tooltip, so panels sharing the index move their rules together.
// With `onSelect`, dragging across the chart shades the runs it spans and, on
// release, hands their first and last index back to zoom into.
export function ChartHover({ n, layout, index, onIndex, tooltip, onSelect }: {
  n: number;
  layout: Layout;
  index: number | null;
  onIndex: (index: number | null) => void;
  tooltip?: ReactNode;
  onSelect?: (first: number, last: number) => void;
}) {
  const [drag, setDrag] = useState<{ from: number; to: number } | null>(null);
  const f = index === null ? null : fractionOf(index, n, layout);
  const at = (e: React.PointerEvent<HTMLDivElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    return box.width > 0 ? indexAt((e.clientX - box.left) / box.width, n, layout) : null;
  };
  const lo = drag ? Math.min(drag.from, drag.to) : 0;
  const hi = drag ? Math.max(drag.from, drag.to) : 0;
  return (
    <div className={`absolute inset-0 z-10 cursor-crosshair touch-pan-y ${onSelect ? "select-none" : ""}`}
      onPointerDown={(e) => {
        if (!onSelect || e.button !== 0) return;
        const i = at(e);
        if (i === null) return;
        // Keeps the drag alive when the pointer leaves the chart mid-drag.
        try {
          e.currentTarget.setPointerCapture?.(e.pointerId);
        } catch {
          // An unknown pointer id: the drag still works while over the chart.
        }
        setDrag({ from: i, to: i });
      }}
      onPointerMove={(e) => {
        const i = at(e);
        if (i === null) return;
        onIndex(i);
        if (drag) setDrag({ ...drag, to: i });
      }}
      onPointerUp={() => {
        if (drag && onSelect && hi > lo) onSelect(lo, hi);
        setDrag(null);
      }}
      onPointerCancel={() => setDrag(null)}
      onPointerLeave={() => {
        if (!drag) onIndex(null);
      }}>
      {drag && hi > lo && (
        <span className="pointer-events-none absolute inset-y-0 border-x border-[var(--fx-info)] bg-[var(--fx-info)]/20" aria-hidden="true"
          style={layout === "slot"
            ? { left: `${(lo / n) * 100}%`, width: `${((hi - lo + 1) / n) * 100}%` }
            : { left: `${fractionOf(lo, n, layout) * 100}%`, width: `${(fractionOf(hi, n, layout) - fractionOf(lo, n, layout)) * 100}%` }} />
      )}
      {f !== null && (
        <span className="pointer-events-none absolute inset-y-0 w-px -translate-x-1/2 bg-[var(--fx-text-muted)] opacity-60"
          style={{ left: `${f * 100}%` }} aria-hidden="true" />
      )}
      {f !== null && tooltip && !drag && (
        <div className="pointer-events-none absolute top-0 z-20 w-max min-w-40 rounded-md bg-popover px-2.5 py-2 text-xs text-popover-foreground shadow-md ring-1 ring-foreground/10"
          style={f > 0.5 ? { right: `calc(${(1 - f) * 100}% + 10px)` } : { left: `calc(${f * 100}% + 10px)` }}>
          {tooltip}
        </div>
      )}
    </div>
  );
}

const pad = (n: number) => String(n).padStart(2, "0");

// Zones whose usual abbreviation Intl does not give in English; it names them
// by offset only (GMT+9).
const ZONE_ABBR: Record<string, string> = {
  "Asia/Seoul": "KST",
  "Asia/Tokyo": "JST",
};

// zoneLabel names the time zone a time is shown in: its abbreviation where one
// is known (KST, UTC, PST), else its offset from UTC (UTC+5:30).
export function zoneLabel(at: Date, timeZone = Intl.DateTimeFormat().resolvedOptions().timeZone): string {
  if (ZONE_ABBR[timeZone]) return ZONE_ABBR[timeZone];
  const name = new Intl.DateTimeFormat("en-US", { timeZone, timeZoneName: "short" })
    .formatToParts(at)
    .find((part) => part.type === "timeZoneName")?.value ?? "";
  return name.replace(/^GMT/, "UTC");
}

// TooltipTime heads a tooltip with the run's local time to the second and the
// zone it is in.
export function TooltipTime({ at }: { at: string }) {
  const d = new Date(at);
  return (
    <div className="mb-1.5 tabular-nums text-muted-foreground">
      {`${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())} ${zoneLabel(d)}`}
    </div>
  );
}

export function TooltipRow({ color, label, value }: { color: string; label: string; value: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-0.5">
      <span className="inline-flex items-center gap-1.5 text-muted-foreground">
        <span className="size-2 rounded-full" style={{ background: color }} aria-hidden="true" />
        {label}
      </span>
      <span className="font-medium tabular-nums">{value}</span>
    </div>
  );
}
