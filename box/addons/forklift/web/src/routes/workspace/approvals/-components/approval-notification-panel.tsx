import { Badge } from "@/components/app-ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { formatMilliseconds } from "@/utils/format-duration";

import type { PackageApproval } from "@/services/v1/openapi-types";

// ApprovalNotificationPanel shows the approval-request alarm's dispatch details:
// when it was sent, whether delivery succeeded, which receivers it went to, and
// how long the send took. Rendered only once an alarm has been dispatched for
// this package. Result and duration appear once the (async, batched) delivery
// completes.
export function ApprovalNotificationPanel({ approval }: { approval: PackageApproval }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const receivers = approval.notified_receivers ?? [];

  if (receivers.length === 0) return null;

  const isDelivered = approval.notify_result === "delivered";
  const hasFailed = approval.notify_result === "failed";

  return (
    <Card size="sm" className="mb-4" data-testid="panel-notification">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("approval.notification")}</h2>
        <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-5 gap-y-2 [&_dd]:m-0 [&_dt]:text-muted-foreground">
          <dt>{t("approval.notify-result")}</dt>
          <dd>
            {approval.notify_result ? (
              <span
                className={
                  hasFailed
                    ? "text-destructive"
                    : isDelivered
                      ? "text-[var(--fx-success)]"
                      : undefined
                }
              >
                {t(isDelivered
                  ? "approval.delivered"
                  : hasFailed
                    ? "approval.failed"
                    : "approval.notify-pending")}
                {approval.notify_detail ? ` (${approval.notify_detail})` : ""}
              </span>
            ) : (
              <span className="text-muted-foreground">{t("approval.notify-pending")}</span>
            )}
          </dd>
          <dt>{t("approval.notified-at")}</dt>
          <dd className="text-muted-foreground">
            {approval.notified_at ? fmtDate(approval.notified_at) : "-"}
          </dd>
          <dt>{t("common.duration")}</dt>
          <dd className="tabular-nums text-muted-foreground">
            {approval.notify_duration_ms ? formatMilliseconds(approval.notify_duration_ms) : "-"}
          </dd>
          <dt>{t("approval.notify-receivers")}</dt>
          <dd className="flex min-w-0 flex-wrap items-center gap-1.5">
            {receivers.map((receiver) => <Badge key={receiver}>{receiver}</Badge>)}
          </dd>
        </dl>
      </CardContent>
    </Card>
  );
}
