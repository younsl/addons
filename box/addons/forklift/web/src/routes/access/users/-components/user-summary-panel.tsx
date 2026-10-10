import type { ReactNode } from "react";

import { Card, CardContent } from "@/components/ui/card";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

import type { User } from "@/services/v1/openapi-types";

// UserSummaryPanel is the AWS-console-style overview: a compact key/value grid
// of the identity's read-only attributes. Username lives in the page header;
// this surfaces the account type, source, status and timestamps at a glance.
export function UserSummaryPanel({ user }: { user: User }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const fmt = (iso: string | null) => fmtDate(iso) || "-";

  return (
    <Card size="sm" className="mb-4" data-testid="panel-summary">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("common.summary")}</h2>
        <dl className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-3">
          <SummaryItem
            label={t("common.type")}
            value={user.robot ? t("user.type-robot") : t("user.type-user")}
          />
          <SummaryItem label={t("common.source")} value={user.source || "-"} />
          <SummaryItem label={t("common.status")}>
            <span className="inline-flex items-center gap-1.5">
              <span
                className={cn(
                  "size-2 rounded-full",
                  user.disabled ? "bg-destructive" : "bg-[var(--fx-success)]",
                )}
              />
              {user.disabled ? t("common.status.disabled") : t("common.status.active")}
              {user.locked && <span className="text-muted-foreground">({t("common.locked")})</span>}
            </span>
          </SummaryItem>
          <SummaryItem label={t("common.email")} value={user.email || "-"} />
          <SummaryItem label={t("common.created")} value={fmt(user.created_at)} />
          <SummaryItem
            label={t("common.last-login")}
            value={
              user.robot
                ? t("user.no-login")
                : user.last_login_at
                  ? fmt(user.last_login_at)
                  : t("common.never")
            }
          />
        </dl>
      </CardContent>
    </Card>
  );
}

function SummaryItem({
  label,
  value,
  children,
}: {
  label: string;
  value?: string;
  children?: ReactNode;
}) {
  return (
    <div className="min-w-0">
      <dt className="text-xs font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 truncate text-sm" title={value}>{children ?? value}</dd>
    </div>
  );
}
