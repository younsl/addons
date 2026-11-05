import { Card, CardContent } from "@/components/ui/card";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { averageLatency, axisTicks, healthSlots, uptimeRatio } from "@/lib/health-timeline";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { useStorageHealth } from "@/routes/admin/-storage/hooks/use-storage-health";
import { formatMilliseconds } from "@/utils/format-duration";

const SLOTS = 60;
const AXIS_TICKS = 5;

const clock = (d: Date, seconds = false) =>
  d.toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    ...(seconds ? { second: "2-digit" } : {}),
    hour12: false,
  });

export function StorageHealthCard() {
  const { t } = useTranslation();
  const { error, health, now } = useStorageHealth();

  const intervalMs = (health?.interval_seconds ?? 60) * 1000;
  const checks = health?.checks ?? [];
  const slots = now ? healthSlots(checks, now, intervalMs, SLOTS) : Array(SLOTS).fill(null);
  const ticks = now ? axisTicks(now, intervalMs, SLOTS, AXIS_TICKS) : [];
  const uptime = uptimeRatio(checks);
  const latency = averageLatency(checks);

  return (
    <Card className="h-full" data-testid="panel-storage-health">
      <CardContent className="p-4">
        <div className="mb-2 flex items-center justify-between gap-3 text-sm">
          <span className="flex items-center gap-2 text-muted-foreground">
            {t("storage.health")}
            <span className="size-1.5 animate-pulse rounded-full bg-[var(--success)]" aria-hidden="true" />
          </span>
          <span className="tabular-nums">
            <span className="text-muted-foreground">{t("storage.health-uptime")} </span>
            <span className="font-semibold">{uptime === null ? "-" : `${(uptime * 100).toFixed(1)}%`}</span>
            {latency !== null && <span className="text-muted-foreground"> · {formatMilliseconds(latency)}</span>}
          </span>
        </div>

        <div className="flex h-5 gap-px" data-testid="storage-health-bars">
          {slots.map((check, i) =>
            check ? (
              <Tooltip key={i}>
                <TooltipTrigger
                  render={
                    <span
                      tabIndex={0}
                      data-ok={check.ok}
                      className={cn(
                        "flex-1 rounded-[1px] transition-opacity hover:opacity-70",
                        check.ok ? "bg-[var(--success)]" : "bg-[var(--fx-severity-critical)]",
                      )}
                    />
                  }
                />
                <TooltipContent>
                  <span className="flex flex-col gap-0.5 tabular-nums">
                    <span>{clock(new Date(check.at), true)}</span>
                    <span>
                      {check.ok ? t("storage.health-ok") : t("storage.health-failed")}
                      {" · "}
                      {formatMilliseconds(check.latency_ms)}
                    </span>
                    {check.error && <span className="break-all">{check.error}</span>}
                  </span>
                </TooltipContent>
              </Tooltip>
            ) : (
              <span key={i} className="flex-1 rounded-[1px] bg-border" />
            ),
          )}
        </div>

        <div className="mt-1 flex justify-between text-[11px] tabular-nums text-muted-foreground" data-testid="storage-health-axis">
          {ticks.map((tick) => (
            <span key={tick.getTime()}>{clock(tick)}</span>
          ))}
        </div>

        {error && <p className="mt-1 text-xs text-[var(--fx-severity-critical)]">{error}</p>}
      </CardContent>
    </Card>
  );
}
