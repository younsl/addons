import { useQuery } from "@tanstack/react-query";
import { HeartPulse } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatMilliseconds } from "@/utils/format-duration";

import type { UpstreamHealth } from "@/services/v1/openapi-types";

// How often the compact (list) form re-probes the upstream so the latency
// reading stays live. Comfortably above the server's probe timeout (8s).
const POLL_MS = 10_000;

// A failed probe is a reading, not an error: the upstream being unreachable is
// exactly what this badge exists to report. The query itself only fails when
// the request never completed, and that is indistinguishable from the upstream
// being down as far as the badge is concerned.
const UNREACHABLE: UpstreamHealth = {
  applicable: true,
  reachable: false,
  error: "check failed",
};

// UpstreamStatus probes a proxy repository's upstream and renders a health
// badge. compact (list view) shows a heart icon coloured by reachability plus
// the live latency, re-probed every POLL_MS; the full form (detail view) also
// shows the status code. withButton adds a "Recheck" action.
export function UpstreamStatus({
  repoId,
  withButton,
  compact,
  poll,
  upstreamUrl,
}: {
  repoId: number;
  withButton?: boolean;
  compact?: boolean;
  // Re-probe every POLL_MS (compact always polls).
  poll?: boolean;
  // When set, the badge is wrapped in a tooltip showing the upstream address and
  // the current reachability status (used by the repository list icon).
  upstreamUrl?: string;
}) {
  const { t } = useTranslation();
  const healthQuery = useQuery({
    ...openApiQueryOptions.getRepositoryUpstreamHealth({ path: { id: repoId } }),
    refetchInterval: compact || poll ? POLL_MS : false,
    // A probe that failed is worth showing at once rather than retried behind
    // the user's back; the poll will try again shortly anyway.
    retry: false,
    // A latency reading is only interesting while it is current.
    staleTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  // While a re-probe is in flight the previous reading stays on screen (only the
  // very first probe shows "checking…"), so the polling list never flashes.
  const health = healthQuery.data ?? (healthQuery.isError ? UNREACHABLE : undefined);
  const isProbing = healthQuery.isFetching;

  // Each state carries a distinct key so React replaces the whole badge subtree
  // on a state change instead of reusing the previous one's DOM nodes. The
  // variants differ in child count (the reachable form appends status/latency),
  // and without a key React's keyless sibling reconciliation can update the dot
  // element but leave the adjacent text node stale - e.g. the green "reachable"
  // dot rendered next to leftover "checking…" text, leaving the badge stuck.
  let badge;
  if (!health) {
    badge = (
      <span key="loading" className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
        <HeartPulse className="size-3.5 shrink-0" aria-hidden="true" /> {t("common.checking")}
      </span>
    );
  } else if (!health.applicable) {
    badge = <span key="na" className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">-</span>;
  } else if (health.reachable) {
    badge = (
      <span
        key="reachable"
        className="inline-flex items-center gap-1 text-xs text-muted-foreground"
        title={t("common.status.reachable")}
      >
        <HeartPulse
          className={cn("size-3.5 shrink-0 text-[var(--fx-success)]", isProbing && "opacity-60")}
          aria-hidden="true"
        />
        <span className="tabular-nums">{formatMilliseconds(health.latency_ms ?? 0)}</span>
        {!compact && <> · {health.status}</>}
      </span>
    );
  } else {
    badge = (
      <span
        key="unreachable"
        className="inline-flex items-center gap-1 text-xs text-muted-foreground"
        title={health.error}
      >
        <HeartPulse
          className={cn("size-3.5 shrink-0 text-[var(--fx-danger)]", isProbing && "opacity-60")}
          aria-hidden="true"
        />
        {t("common.status.unreachable")}
      </span>
    );
  }

  // Plain-language status line for the tooltip: reachability plus latency and
  // (when reachable) the HTTP status; the error message when unreachable.
  const statusText = !health
    ? t("common.checking")
    : !health.applicable
      ? "-"
      : health.reachable
        ? `${t("common.status.reachable")}, ${formatMilliseconds(health.latency_ms ?? 0)}${health.status ? `, ${health.status}` : ""}`
        : `${t("common.status.unreachable")}${health.error ? `: ${health.error}` : ""}`;

  // When an upstream address is supplied, wrap the badge in a tooltip that shows
  // the address and the current status on hover/focus.
  const withTooltip = upstreamUrl ? (
    <Tooltip>
      <TooltipTrigger
        render={
          <span tabIndex={0} className="inline-flex" aria-label={`${upstreamUrl}: ${statusText}`} />
        }
      >
        {badge}
      </TooltipTrigger>
      <TooltipContent className="max-w-none">
        <span className="flex flex-col gap-0.5">
          <span className="whitespace-nowrap font-mono text-xs">{upstreamUrl}</span>
          <span className="whitespace-nowrap text-xs">{statusText}</span>
        </span>
      </TooltipContent>
    </Tooltip>
  ) : (
    badge
  );

  if (!withButton) return withTooltip;

  return (
    <span className="flex items-center gap-2.5 max-sm:flex-col max-sm:items-stretch">
      {withTooltip}
      <Button
        variant="outline"
        type="button"
        onClick={() => healthQuery.refetch()}
        disabled={isProbing}
      >
        {t("common.recheck")}
      </Button>
    </span>
  );
}
