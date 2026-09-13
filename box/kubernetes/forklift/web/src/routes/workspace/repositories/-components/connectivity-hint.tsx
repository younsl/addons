import { useTranslation } from "@/lib/i18n";
import { formatMilliseconds } from "@/utils/format-duration";

import type { UpstreamHealth } from "@/services/v1/openapi-types";

// ConnectivityHint renders the live result of the debounced upstream probe
// under the URL field: a "checking" line, then reachable or unreachable. It is
// advisory - an unreachable upstream does not block creating the repository,
// since the address may be behind something not yet routable.
export function ConnectivityHint({
  isChecking,
  health,
  hasUrl,
}: {
  isChecking: boolean;
  health: UpstreamHealth | null;
  hasUrl: boolean;
}) {
  const { t } = useTranslation();

  if (!hasUrl) return null;
  if (isChecking) {
    return (
      <p className="mt-1.5 text-sm text-muted-foreground">{t("common.checking-connectivity")}</p>
    );
  }
  if (!health) return null;

  if (health.reachable) {
    return (
      <p className="mt-1.5 text-sm text-emerald-300">
        ✓ Reachable - HTTP {health.status}
        {health.latency_ms != null && ` (${formatMilliseconds(health.latency_ms)})`}
      </p>
    );
  }

  return (
    <p className="mt-1.5 text-sm text-destructive">
      ✗ Unreachable{health.error ? ` - ${health.error}` : ""}
    </p>
  );
}
