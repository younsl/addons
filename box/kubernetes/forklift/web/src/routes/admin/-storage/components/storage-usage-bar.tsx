import { Card, CardContent } from "@/components/ui/card";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { USAGE_CRITICAL_PCT, USAGE_WARNING_PCT, usageTone } from "@/lib/usage";
import { formatFileSize } from "@/utils/format-file-size";

const TONE_CLASS = {
  critical: "bg-[var(--fx-severity-critical)]",
  warning: "bg-[var(--fx-warning)]",
  ok: "bg-[var(--success)]",
} as const;

export function StorageUsageBar({
  usedBytes,
  totalBytes,
  usagePct,
}: {
  usedBytes: number;
  totalBytes: number;
  usagePct: number;
}) {
  const { t } = useTranslation();

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
    <Card className="mb-3">
      <CardContent className="p-4">
        <div className="mb-2 flex items-center justify-between text-sm">
          <span className="text-muted-foreground">{t("storage.usage")}</span>
          <span className="font-semibold tabular-nums">
            {formatFileSize(usedBytes)} / {formatFileSize(totalBytes)} ({usagePct.toFixed(1)}%)
          </span>
        </div>
        <div className="relative h-2.5 w-full overflow-hidden rounded-full bg-border">
          <div
            className={cn("h-full rounded-full", TONE_CLASS[usageTone(usagePct)])}
            // Clamped: a backend reporting more used than capacity would
            // otherwise overflow the track rather than reading as full.
            style={{ width: `${Math.min(100, Math.max(0, usagePct))}%` }}
          />
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
                {mark.label} {mark.pct}%
              </span>
            </span>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
