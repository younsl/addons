import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { SeverityBadge } from "@/components/app-ui/severity-badge";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

// Severity order (worst first) and segment colours for the mini bar.
export const SEV_ORDER = ["critical", "high", "medium", "low"] as const;

const SEV_RANK: Record<string, number> = Object.fromEntries(
  SEV_ORDER.map((severity, index) => [severity, index]),
);

// Sorts severities worst-first. "none" (clean) ranks after the known levels,
// and unscanned (undefined) sinks to the bottom via the comparator - an
// unscanned package is not safer than a clean one, only unknown.
export const sevRank = (severity?: string) =>
  severity === undefined ? undefined : SEV_RANK[severity] ?? SEV_ORDER.length;

export const SEV_COLOR: Record<string, string> = {
  critical: "var(--fx-severity-critical)",
  high: "var(--fx-severity-high)",
  medium: "var(--fx-severity-medium)",
  low: "var(--fx-severity-low)",
};

const SEV_BG_CLASS: Record<string, string> = {
  critical: "bg-[var(--fx-severity-critical)]",
  high: "bg-[var(--fx-severity-high)]",
  medium: "bg-[var(--fx-severity-medium)]",
  low: "bg-[var(--fx-severity-low)]",
};

export type Advisory = { id: string; severity: string; score?: string };

// SeverityBar renders the per-level advisory counts as a segmented stacked bar
// (segment width proportional to count, coloured by severity). size "sm" (the
// list) shows a narrow bar with the bare count on the right; size "lg" (the
// detail page) shows a wide bar with an "N vulns" label. "not scanned" and
// "clean" reuse the badge styling; a scanned result without a per-level
// histogram (older scan) falls back to the single badge.
//
// Hovering the bar opens a detailed popover: a wider segmented bar plus a
// per-severity count breakdown. The popover is fixed-positioned from the
// trigger's rect so it is never clipped by a scrolling table container.
export function SeverityBar({
  severity,
  counts,
  scope,
  source,
  scannedAt,
  advisories,
  size = "sm",
}: {
  severity?: string;
  counts?: Record<string, number>;
  scope?: string;
  source?: string;
  scannedAt?: string | null;
  // Per-advisory detail when available; powers the CVSS max/min line.
  advisories?: Advisory[];
  size?: "sm" | "lg";
}) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const [isOpen, setIsOpen] = useState(false);
  const triggerRef = useRef<HTMLSpanElement>(null);
  const [position, setPosition] = useState<{ top: number; left: number; above: boolean } | null>(null);

  const updatePosition = useCallback(() => {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (!rect) return;

    const tooltipWidth = 240;
    const gap = 8;
    const margin = 12;
    const left = Math.min(
      Math.max(rect.left + rect.width / 2, margin + tooltipWidth / 2),
      window.innerWidth - margin - tooltipWidth / 2,
    );
    const belowTop = rect.bottom + gap;
    // Flip above only when there is room there and not below; a popover half
    // off-screen is worse than one that overlaps a little.
    const above = belowTop + 180 > window.innerHeight && rect.top > 180;

    setPosition({ top: above ? rect.top - gap : belowTop, left, above });
  }, []);

  useLayoutEffect(() => {
    if (!isOpen) return;

    updatePosition();
    window.addEventListener("resize", updatePosition);
    // Capture phase: the table's own scroll container has to move it too.
    window.addEventListener("scroll", updatePosition, true);

    return () => {
      window.removeEventListener("resize", updatePosition);
      window.removeEventListener("scroll", updatePosition, true);
    };
  }, [isOpen, updatePosition]);

  // Unscanned has no provenance to show, so it stays a plain muted label.
  if (severity === undefined) {
    return <span className="text-muted-foreground">{t("approval.not-scanned-short")}</span>;
  }

  const perLevel = counts ?? {};
  const total = SEV_ORDER.reduce((sum, level) => sum + (perLevel[level] ?? 0), 0);
  // Clean = scanned with no advisories (severity "none"), or a scanned result
  // without a per-level histogram. Both render the green badge but still open a
  // popover with the scan provenance.
  const isClean = severity === "none" || total === 0;
  // Numeric CVSS scores across the advisories, for the max/min line. Scores are
  // optional per advisory (OSV does not always carry one), so filter NaN.
  const scores = (advisories ?? [])
    .map((advisory) => parseFloat(advisory.score ?? ""))
    .filter((score) => !Number.isNaN(score));
  const suffix = scope === "package" ? " · pkg" : "";
  const label = size === "lg" ? `${total} vuln${total !== 1 ? "s" : ""}${suffix}` : `${total}`;

  const open = () => {
    updatePosition();
    setIsOpen(true);
  };

  const segments = () =>
    SEV_ORDER.flatMap((level) =>
      Array.from({ length: perLevel[level] ?? 0 }, (_, index) => (
        <span key={`${level}-${index}`} className={cn("h-full min-w-[3px] flex-1", SEV_BG_CLASS[level])} />
      )),
    );

  const tooltip =
    isOpen && position && typeof document !== "undefined"
      ? createPortal(
          <span
            className="pointer-events-none fixed z-[1000] flex w-[240px] max-w-[calc(100vw-24px)] flex-col gap-[9px] rounded-[var(--radius)] border border-border bg-[var(--panel-3)] px-3 py-2.5 text-foreground shadow-[var(--fx-overlay-shadow)]"
            style={{
              left: position.left,
              top: position.top,
              transform: position.above ? "translate(-50%, -100%)" : "translateX(-50%)",
            }}
            role="tooltip"
          >
            <span className="text-xs font-semibold">
              {isClean
                ? t("approval.no-advisories-short")
                : `${total} vulnerabilit${total === 1 ? "y" : "ies"}`}
              {scope === "package" ? " · package-level" : ""}
            </span>
            {!isClean && (
              <span className="inline-flex h-2.5 w-full overflow-hidden rounded-[5px] bg-border">
                {segments()}
              </span>
            )}
            {!isClean && (
              <span className="flex min-w-[150px] flex-col gap-1">
                {SEV_ORDER.map((level) => (
                  <span key={level} className="flex items-center gap-[9px] text-xs leading-[1.2]">
                    <span className={cn("size-[9px] shrink-0 rounded-[2px]", SEV_BG_CLASS[level])} />
                    <span className="capitalize text-foreground">{level}</span>
                    <span className="ml-auto font-semibold tabular-nums">{perLevel[level] ?? 0}</span>
                  </span>
                ))}
              </span>
            )}
            {!isClean && scores.length > 0 && <CvssRange scores={scores} />}
            <span className="border-t border-border pt-[7px] text-[11px] text-muted-foreground">
              Source {source || "OSV"} · scanned {scannedAt ? fmtDate(scannedAt) : "n/a"}
            </span>
          </span>,
          document.body,
        )
      : null;

  return (
    <span
      ref={triggerRef}
      className={cn(
        "relative inline-flex cursor-help items-center gap-2 outline-none",
        size === "lg" && "gap-3",
      )}
      tabIndex={0}
      onMouseEnter={open}
      onMouseLeave={() => setIsOpen(false)}
      onFocus={open}
      onBlur={() => setIsOpen(false)}
    >
      {isClean ? (
        <SeverityBadge severity="none">{t("approval.clean-short")}{suffix}</SeverityBadge>
      ) : (
        <>
          <span
            className={cn(
              "inline-flex overflow-hidden rounded bg-border",
              size === "lg" ? "h-4 w-[280px] rounded-lg max-[760px]:w-[180px]" : "h-1.5 w-[54px]",
            )}
          >
            {segments()}
          </span>
          <span className={cn("text-xs text-muted-foreground tabular-nums", size === "lg" && "text-sm")}>
            {label}
          </span>
        </>
      )}
      {tooltip}
    </span>
  );
}

// cvssLevel maps a CVSS score to its severity band (CVSS v3 qualitative
// rating), reusing the severity colours so the meter reads like the bar above.
const cvssLevel = (score: number) =>
  score >= 9 ? "critical" : score >= 7 ? "high" : score >= 4 ? "medium" : "low";

// CVSS v3 qualitative bands, as [from, to) track segments of the 0-10 meter.
const CVSS_BANDS = [
  { level: "low", from: 0, to: 4 },
  { level: "medium", from: 4, to: 7 },
  { level: "high", from: 7, to: 9 },
  { level: "critical", from: 9, to: 10 },
] as const;

// CvssRange draws the advisory score spread as a 0-10 meter. The track shows
// the CVSS severity bands as faint colour segments (with boundary ticks at 4, 7
// and 9), a solid band spans the lowest to the highest score, and endpoint dots
// are coloured by each score's own band. A single score collapses to one dot.
function CvssRange({ scores }: { scores: number[] }) {
  const min = Math.min(...scores);
  const max = Math.max(...scores);
  const pct = (score: number) => `${(score / 10) * 100}%`;
  const isSingle = scores.length === 1 || min === max;

  return (
    <span className="flex flex-col gap-1">
      <span className="flex items-baseline justify-between text-[11px] leading-[1.2]">
        <span className="text-muted-foreground">CVSS</span>
        <span className="font-semibold tabular-nums">
          {isSingle ? max.toFixed(1) : <>{min.toFixed(1)} – {max.toFixed(1)}</>}
        </span>
      </span>
      <span className="relative my-1 block h-1.5 w-full overflow-hidden rounded-[3px] bg-border">
        {CVSS_BANDS.map((band) => (
          <span
            key={band.level}
            className="absolute top-0 h-full opacity-25"
            style={{
              left: pct(band.from),
              width: pct(band.to - band.from),
              background: SEV_COLOR[band.level],
            }}
          />
        ))}
        {!isSingle && (
          <span
            className="absolute top-0 h-full rounded-[3px] opacity-70"
            style={{
              left: pct(min),
              width: pct(max - min),
              background: `linear-gradient(90deg, ${SEV_COLOR[cvssLevel(min)]}, ${SEV_COLOR[cvssLevel(max)]})`,
            }}
          />
        )}
        {(isSingle ? [max] : [min, max]).map((score, index) => (
          <span
            key={index}
            className="absolute top-1/2 size-[9px] -translate-x-1/2 -translate-y-1/2 rounded-full border border-[var(--panel-3)]"
            style={{ left: pct(score), background: SEV_COLOR[cvssLevel(score)] }}
          />
        ))}
      </span>
      {/* Band boundaries under the track: 0 · 4 · 7 · 9 · 10. */}
      <span className="relative block h-[11px] text-[10px] leading-none text-muted-foreground tabular-nums">
        <span className="absolute left-0">0</span>
        {[4, 7, 9].map((tick) => (
          <span key={tick} className="absolute -translate-x-1/2" style={{ left: pct(tick) }}>
            {tick}
          </span>
        ))}
        <span className="absolute right-0">10</span>
      </span>
    </span>
  );
}
