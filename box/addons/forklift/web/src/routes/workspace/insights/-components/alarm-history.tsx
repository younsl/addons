import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { useTranslation } from "@/lib/i18n";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import type { AlarmDelivery } from "@/services/v1/openapi-types";
import { TooltipRow, TooltipTime } from "./chart-hover";
import { MetricsPanels, type ChartPoint, type Series, type ZoomRange } from "./check-history";

type Feature = "insights" | "coverage";

const DAYS = 30;
const PARAMS = { query: { days: DAYS } };

const RESULT_COLOR: Record<string, string> = {
  delivered: "var(--fx-success)",
  failed: "var(--fx-danger)",
};

// alarmHistoryKey is the query a manual send refreshes, so the chart shows the
// delivery it just made.
export function alarmHistoryKey(feature: Feature) {
  return feature === "insights"
    ? openApiQueryKeys.getInsightsNotificationHistory(PARAMS)
    : openApiQueryKeys.getCoverageNotificationHistory(PARAMS);
}

// AlarmHistory charts how an alarm went out over the last 30 days with the
// same Metrics panels a check page uses: whether each delivery landed on the
// left, how many findings or projects it listed on the right. One point is one
// delivery to one receiver, scheduled or sent by hand.
export function AlarmHistory({ feature }: { feature: Feature }) {
  const { t } = useTranslation();
  const [range, setRange] = useState<ZoomRange>({});
  const query = useQuery({
    ...(feature === "insights"
      ? openApiQueryOptions.getInsightsNotificationHistory(PARAMS)
      : openApiQueryOptions.getCoverageNotificationHistory(PARAMS)),
    meta: { suppressGlobalErrorToast: true },
  });
  if (query.isError) return null;
  const deliveries: AlarmDelivery[] = query.data?.deliveries ?? [];
  const points: ChartPoint[] = deliveries.map((d) => ({
    generated_at: d.sent_at,
    status: d.ok ? "delivered" : "failed",
    flagged: d.items,
  }));
  const byTime = new Map(deliveries.map((d) => [d.sent_at, d]));
  const label = (status: string) => t(status === "delivered" ? "alarm-history.delivered" : "alarm-history.failed");
  const countTitle = t(feature === "insights" ? "alarm-history.findings" : "alarm-history.projects");
  const series: Series = {
    color: (status) => RESULT_COLOR[status] ?? "var(--fx-border-subtle)",
    label,
    urgency: ["failed", "delivered"],
    tooltip: (point) => {
      const d = byTime.get(point.generated_at);
      return (
        <>
          <TooltipTime at={point.generated_at} />
          <TooltipRow color={series.color(point.status)} label={t("alarm-history.result")} value={label(point.status)} />
          <TooltipRow color="var(--fx-accent-ink)" label={countTitle} value={point.flagged.toLocaleString()} />
          {d && (
            <>
              <TooltipRow color="transparent" label={t("alarm-history.receiver")} value={d.receiver} />
              <TooltipRow color="transparent" label={t("alarm-history.trigger")}
                value={t(d.trigger === "manual" ? "alarm-history.trigger-manual" : "alarm-history.trigger-schedule")} />
              {d.error && <TooltipRow color="transparent" label={t("alarm-history.error")} value={d.error} />}
            </>
          )}
        </>
      );
    },
  };
  return (
    <MetricsPanels title={t("alarm-history.title")} points={points} series={series}
      statusTitle={t("alarm-history.status")} countTitle={countTitle} days={DAYS}
      runs={{ window: "alarm-history.window", zoomed: "alarm-history.zoomed" }}
      empty={query.isLoading ? t("common.loading") : t("alarm-history.empty")} range={range} onRange={setRange} />
  );
}
