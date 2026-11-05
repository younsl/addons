import { Card, CardContent } from "@/components/ui/card";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import {
  USAGE_CRITICAL_PCT,
  USAGE_WARNING_PCT,
  segmentWidths,
  usageSegments,
  usageTone,
  type UsageSegment,
} from "@/lib/usage";
import type { FormatUsage } from "@/services/v1/openapi-types";
import { formatFileSize } from "@/utils/format-file-size";

// The fill no longer carries the level, so the figure in the header does once
// it crosses a threshold.
const TONE_TEXT = {
  critical: "text-[var(--fx-severity-critical)]",
  warning: "text-[var(--fx-warning)]",
  ok: "",
} as const;

const FORMAT_NAME: Record<string, string> = {
  maven: "Maven",
  npm: "npm",
  cargo: "Cargo",
  go: "Go",
  pypi: "PyPI",
  raw: "Raw",
  oci: "OCI",
};

// Keys of the parts that are not a format, with their legend name and hint.
const PART_TEXT = {
  "forklift-other": ["storage.usage-forklift-other", "storage.usage-forklift-other-hint"],
  "cluster-other": ["storage.usage-cluster-other", "storage.usage-cluster-other-hint"],
  system: ["storage.usage-system", "storage.usage-system-hint"],
  "cluster-rest": ["storage.usage-cluster-rest", "storage.usage-cluster-rest-hint"],
} as const;

// System data is drawn hatched over its colour, so it reads as disk space
// rather than as one more kind of stored object.
const fill = (s: { color: string; hatched?: boolean }) =>
  s.hatched
    ? { background: `repeating-linear-gradient(135deg, ${s.color} 0 2px, transparent 2px 4px)` }
    : { background: s.color };

const pct = (bytes: number, total: number) => (total > 0 ? `${((bytes / total) * 100).toFixed(1)}%` : "-");

export function StorageUsageBar({
  usedBytes,
  totalBytes,
  availableBytes,
  usagePct,
  blobBytes,
  formats,
  logicalBytes,
}: {
  usedBytes: number;
  totalBytes: number;
  availableBytes: number;
  usagePct: number;
  blobBytes: number;
  formats: FormatUsage[];
  logicalBytes?: number;
}) {
  const { t } = useTranslation();
  const segments = usageSegments(formats, blobBytes, usedBytes, logicalBytes);
  const widths = segmentWidths(segments, totalBytes, usagePct);
  // The bar puts what is not forklift's at the far left, so forklift's own
  // parts sit next to the free space they grow into.
  const outside = new Set(["system", "cluster-other", "cluster-rest"]);
  const order = segments.map((_, i) => i);
  const bar = [...order.filter((i) => outside.has(segments[i].key)).reverse(), ...order.filter((i) => !outside.has(segments[i].key))];
  const part = (s: UsageSegment) => PART_TEXT[s.key as keyof typeof PART_TEXT] ?? PART_TEXT["cluster-rest"];
  const name = (s: UsageSegment) => (s.format ? FORMAT_NAME[s.format] ?? s.format : t(part(s)[0]));
  const hint = (s: UsageSegment) =>
    s.format ? t("storage.usage-blobs").replace("{count}", (s.blobCount ?? 0).toLocaleString()) : t(part(s)[1]);

  // The two points where the bar changes colour, each drawn as a notch in the
  // track plus a pointer and caption beneath it.
  const marks = [
    {
      pct: USAGE_WARNING_PCT,
      label: t("storage.usage-warning"),
      pointer: "border-b-[var(--fx-warning)]",
      text: "text-[var(--fx-warning)]",
    },
    {
      pct: USAGE_CRITICAL_PCT,
      label: t("storage.usage-critical"),
      pointer: "border-b-[var(--fx-severity-critical)]",
      text: "text-[var(--fx-severity-critical)]",
    },
  ];

  return (
    <Card className="h-full">
      <CardContent className="p-4">
        <div className="mb-2 flex items-center justify-between text-sm">
          <span className="text-muted-foreground">{t("storage.usage")}</span>
          <span className="font-semibold tabular-nums">
            {formatFileSize(usedBytes)} / {formatFileSize(totalBytes)}{" "}
            <span className={TONE_TEXT[usageTone(usagePct)]}>({usagePct.toFixed(1)}%)</span>
          </span>
        </div>
        <div className="relative flex h-2.5 w-full gap-px overflow-hidden rounded-full bg-border" data-testid="storage-usage-segments">
          {bar.map((i) => segments[i]).map((s, j) => (
            <Tooltip key={s.key}>
              <TooltipTrigger
                render={
                  <span
                    tabIndex={0}
                    data-segment={s.key}
                    aria-label={`${name(s)} ${formatFileSize(s.bytes)} ${pct(s.bytes, totalBytes)}`}
                    className="h-full shrink-0 transition-opacity hover:opacity-80 focus-visible:opacity-80 focus-visible:outline-none"
                    style={{ width: `${widths[bar[j]]}%`, ...fill(s) }}
                  />
                }
              />
              <TooltipContent>
                <span className="flex flex-col gap-0.5 tabular-nums">
                  <span className="font-medium">{name(s)}</span>
                  <span>
                    {formatFileSize(s.bytes)} · {pct(s.bytes, totalBytes)} {t("storage.usage-of-capacity")}
                  </span>
                  <span>
                    {pct(s.bytes, usedBytes)} {t("storage.usage-of-used")} · {hint(s)}
                  </span>
                </span>
              </TooltipContent>
            </Tooltip>
          ))}
          {/* Threshold notches over the fill, so the warning and critical bands
              stay readable at any level. */}
          {marks.map((mark) => (
            <span
              key={mark.pct}
              aria-hidden="true"
              className="absolute top-0 h-full w-px bg-card"
              style={{ left: `${mark.pct}%` }}
            />
          ))}
        </div>
        {/* Pointer and caption centred on each notch, so the label names the
            line directly above it rather than sitting in a legend the eye has
            to match up by colour alone. */}
        <div className="relative mt-1 h-7 text-[11px] tabular-nums">
          {marks.map((mark) => (
            <span
              key={mark.pct}
              className="absolute flex -translate-x-1/2 flex-col items-center gap-0.5"
              style={{ left: `${mark.pct}%` }}
            >
              <span
                aria-hidden="true"
                className={cn("size-0 border-x-4 border-x-transparent border-b-[5px]", mark.pointer)}
              />
              <span className={cn("whitespace-nowrap", mark.text)}>
                {/* The two captions sit 20% apart, which is too narrow for both
                    words on a phone. The colour still names the level. */}
                <span className="max-sm:sr-only">{mark.label} </span>
                {mark.pct}%
              </span>
            </span>
          ))}
        </div>
        <ul className="m-0 mt-2 grid list-none gap-x-4 gap-y-1.5 p-0 text-xs sm:grid-cols-2" aria-label={t("storage.usage-legend")}
          data-testid="storage-usage-legend">
          {[...segments.map((s) => ({ key: s.key, label: name(s), bytes: s.bytes, color: s.color, hatched: s.hatched })),
            { key: "free", label: t("storage.usage-free"), bytes: availableBytes, color: "", hatched: false }].map((item) => (
            <li key={item.key} className="flex min-w-0 items-center gap-2">
              <span aria-hidden="true"
                className={cn("size-2.5 shrink-0 rounded-sm", !item.color && "border border-[var(--fx-border-strong)] bg-border")}
                style={item.color ? fill(item) : undefined} />
              <span className="min-w-0 truncate text-muted-foreground">{item.label}</span>
              <span className="ml-auto shrink-0 tabular-nums">
                {formatFileSize(item.bytes)}
                <span className="ml-1.5 inline-block w-12 text-right text-muted-foreground">{pct(item.bytes, totalBytes)}</span>
              </span>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}
