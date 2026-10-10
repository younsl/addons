import type { ReactNode } from "react";

import { Badge } from "@/components/app-ui/badge";
import { CopyIconButton } from "@/components/app-ui/copy-button";
import {
  Table,
  TableBody,
  TableCell,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import { formatUptime } from "@/routes/admin/-ha/utils/ha-status";

import type { HAStatus } from "@/services/v1/openapi-types";

// ValueCell is one status value with a copy action pinned to its right. Every
// value in this table is something an operator ends up pasting somewhere - a
// pod name into kubectl, a lease into a ticket - so they all carry one.
function ValueCell({
  copy,
  className,
  children,
}: {
  copy?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <TableCell className={className}>
      {/* The button sits immediately after the value, not at the cell edge: it
          belongs to the value, and a gap the width of the column would read as
          an unrelated control. */}
      <span className="inline-flex min-w-0 items-center gap-1">
        <span className="min-w-0">{children}</span>
        {/* Nothing to paste when the value is absent, so no button either. */}
        {copy && <CopyIconButton value={copy} />}
      </span>
    </TableCell>
  );
}

export function HaStatusTable({ status }: { status: HAStatus }) {
  const { t } = useTranslation();

  return (
    <TableWrap className="mt-4">
      <Table>
        <TableBody>
          <TableRow>
            <TableCell className="w-44 text-muted-foreground">{t("common.mode")}</TableCell>
            <ValueCell copy={status.mode}>{status.mode}</ValueCell>
          </TableRow>
          <TableRow>
            <TableCell className="text-muted-foreground">{t("ha.storage-backend")}</TableCell>
            <ValueCell copy={status.backend}>{status.backend}</ValueCell>
          </TableRow>
          <TableRow>
            <TableCell className="text-muted-foreground">{t("ha.leader-election")}</TableCell>
            <ValueCell copy={status.enabled ? "enabled" : "disabled"}>
              {status.enabled ? t("common.status.enabled") : "disabled (single instance)"}
            </ValueCell>
          </TableRow>
          <TableRow>
            <TableCell className="text-muted-foreground">{t("ha.this-pod")}</TableCell>
            <ValueCell className="font-mono text-xs" copy={status.identity}>
              {status.identity || "-"}
            </ValueCell>
          </TableRow>
          <TableRow>
            <TableCell className="text-muted-foreground">{t("common.role")}</TableCell>
            <ValueCell copy={status.role}>
              <span className="inline-flex min-w-0 items-center gap-2 max-sm:flex-wrap">
                <Badge variant={status.is_leader ? "success" : "outline"}>
                  {status.role || "-"}
                </Badge>
                {status.is_leader && (
                  <span className="text-sm text-muted-foreground">{t("ha.serving-traffic")}</span>
                )}
              </span>
            </ValueCell>
          </TableRow>
          <TableRow>
            <TableCell className="text-muted-foreground">{t("ha.current-leader")}</TableCell>
            <ValueCell className="font-mono text-xs" copy={status.leader}>
              {status.leader || "-"}
            </ValueCell>
          </TableRow>
          {status.version && (
            <TableRow>
              <TableCell className="text-muted-foreground">{t("common.version")}</TableCell>
              <ValueCell copy={status.version}>{status.version}</ValueCell>
            </TableRow>
          )}
          {status.runtime && (
            <TableRow>
              <TableCell className="text-muted-foreground">{t("ha.runtime")}</TableCell>
              <ValueCell copy={status.runtime}>{status.runtime}</ValueCell>
            </TableRow>
          )}
          {status.started_at && (
            <TableRow>
              <TableCell className="text-muted-foreground">{t("ha.uptime")}</TableCell>
              {/* Copies the start timestamp, not the ticking uptime: the elapsed
                  figure is stale the moment it is pasted. */}
              <ValueCell copy={status.started_at}>
                {formatUptime(status.started_at)}{" "}
                <span className="text-muted-foreground">
                  · since {status.started_at.slice(0, 19).replace("T", " ")}
                </span>
              </ValueCell>
            </TableRow>
          )}
          {status.lease_name && (
            <TableRow>
              <TableCell className="text-muted-foreground">{t("ha.lease")}</TableCell>
              <ValueCell className="font-mono text-xs" copy={status.lease_name}>
                {status.lease_name}
              </ValueCell>
            </TableRow>
          )}
          {typeof status.fencing_token === "number" && status.fencing_token > 0 && (
            <TableRow>
              <TableCell className="text-muted-foreground">{t("ha.fencing-token")}</TableCell>
              <ValueCell copy={String(status.fencing_token)}>{status.fencing_token}</ValueCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </TableWrap>
  );
}
