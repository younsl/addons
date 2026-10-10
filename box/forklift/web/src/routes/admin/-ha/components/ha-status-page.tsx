import { useState } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { useTranslation } from "@/lib/i18n";
import { HaArchitecture } from "@/routes/admin/-ha/components/ha-architecture";
import { HaStatusTable } from "@/routes/admin/-ha/components/ha-status-table";
import { useHaStatus } from "@/routes/admin/-ha/hooks/use-ha-status";
import { storageUsage } from "@/routes/admin/-ha/utils/ha-status";

// HaStatusPage renders the live HA cluster topology and status, plus the
// manual-failover (step-down) control. The step-down danger zone only shows for
// the active leader.
export function HaStatusPage() {
  const { t } = useTranslation();
  const [isConfirmingStepDown, setIsConfirmingStepDown] = useState(false);
  const ha = useHaStatus();
  const { status } = ha;

  // Manual failover only makes sense in HA mode when this pod is the active
  // leader; a standby has nothing to release.
  const canStepDown = Boolean(status?.enabled && status?.is_leader);

  return (
    <div data-testid="page-ha">
      <PageHeader title={t("ha.title")} />
      <PageDescription>{t("ha.description")}</PageDescription>

      <Card size="sm" className="mb-4" data-testid="panel-cluster-status">
        <CardContent>
          <div className="mb-4 flex min-w-0 items-center justify-between gap-3 max-sm:flex-col max-sm:flex-wrap max-sm:items-start">
            <h2 className="m-0 text-base font-semibold">{t("ha.cluster-status")}</h2>
            <div className="flex min-w-0 items-center gap-2 max-sm:w-full max-sm:flex-wrap max-sm:justify-between">
              <span
                className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted px-[9px] py-0.5 text-[11px] tabular-nums text-muted-foreground"
                title={t("ha.auto-refresh-title")}
              >
                <span
                  className="size-1.5 flex-none rounded-full bg-[var(--fx-success)] [animation:refresh-pulse_1.4s_ease-in-out_infinite] motion-reduce:animate-none"
                  aria-hidden="true"
                />
                auto-refresh {ha.secondsLeft}s
              </span>
              <Button variant="outline" type="button" onClick={ha.refresh}>
                {t("common.refresh")}
              </Button>
            </div>
          </div>

          {ha.error && <Alert className="mb-4">{ha.error}</Alert>}
          {ha.notice && <div className="mb-4 text-sm text-muted-foreground">{ha.notice}</div>}
          {!status ? (
            <p className="m-0 text-sm text-muted-foreground">{t("common.loading")}</p>
          ) : (
            <>
              <HaArchitecture status={status} usage={storageUsage(ha.usageSource)} />
              <HaStatusTable status={status} />
            </>
          )}

          <p className="mb-0 mt-4 text-sm leading-relaxed text-muted-foreground">
            In HA only the leader serves traffic; standby pods stay ready and take over
            automatically on failover. The fencing token guards object-storage metadata against
            a superseded leader.
          </p>
        </CardContent>
      </Card>

      {canStepDown && (
        <Card size="sm" className="mb-4 ring-destructive" data-testid="panel-danger-zone">
          <CardContent>
            <h2 className="m-0 mb-3 text-base font-semibold text-destructive">
              {t("common.danger-zone")}
            </h2>
            <div className="flex min-w-0 items-center justify-between gap-4 max-sm:flex-col max-sm:flex-wrap max-sm:items-start">
              <p className="m-0 text-sm leading-relaxed text-muted-foreground">
                {t("ha.failover-note")}
              </p>
              <Button
                variant="destructive"
                type="button"
                disabled={ha.isSteppingDown}
                onClick={() => setIsConfirmingStepDown(true)}
              >
                {ha.isSteppingDown ? t("ha.stepping-down") : t("ha.step-down-button")}
              </Button>
            </div>
          </CardContent>
        </Card>
      )}

      <ConfirmModal
        open={isConfirmingStepDown}
        title={t("ha.step-down-confirm-title")}
        message={t("ha.step-down-confirm-message")}
        confirmLabel={t("ha.step-down")}
        danger
        onConfirm={() => { setIsConfirmingStepDown(false); ha.stepDown(); }}
        onCancel={() => setIsConfirmingStepDown(false)}
      />
    </div>
  );
}
