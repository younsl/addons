import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowRight, ChevronRight } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Alert } from "@/components/app-ui/alert";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { MigrationDetail } from "@/routes/admin/-storage/components/migration-detail";
import { Location } from "@/routes/admin/-storage/components/provider-logo";
import { formatMilliseconds } from "@/utils/format-duration";
import { formatFileSize } from "@/utils/format-file-size";

import type { MigrationCheckStatus, MigrationSummary } from "@/services/v1/openapi-types";

const outcomeVariant = {
  succeeded: "success",
  failed: "destructive",
  dry_run: "default",
} as const;

export function OutcomeBadge({ outcome }: { outcome: MigrationSummary["outcome"] }) {
  const { t } = useTranslation();
  return (
    <Badge variant={outcomeVariant[outcome]} data-testid={`migration-outcome-${outcome}`}>
      {t(`storage.migration-outcome-${outcome}` as MessageKey)}
    </Badge>
  );
}

export const statusDot = {
  pass: "bg-[var(--fx-success)]",
  warn: "bg-accent-ink",
  fail: "bg-destructive",
  skip: "bg-muted-foreground/30",
} as const;

export function formatters(language: string) {
  const locale = language === "ko" ? "ko-KR" : "en-US";
  return {
    date: new Intl.DateTimeFormat(locale, { year: "numeric", month: "2-digit", day: "2-digit" }),
    time: new Intl.DateTimeFormat(locale, {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
      timeZoneName: "short",
    }),
    full: new Intl.DateTimeFormat(locale, {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
      timeZoneName: "short",
    }),
  };
}

function CheckDots({ label, checks }: { label: string; checks: MigrationCheckStatus[] }) {
  const failed = checks.filter((c) => c.status === "fail").map((c) => c.id);
  return (
    <div className="flex items-center gap-2 whitespace-nowrap">
      <span className="w-4 font-mono text-[11px] text-muted-foreground">{label}</span>
      <Tooltip>
        <TooltipTrigger render={<span className="flex gap-[2px] py-1" />}>
          {checks.map((c) => (
            <span key={c.id} className={cn("size-[7px] rounded-[1.5px]", statusDot[c.status])} />
          ))}
        </TooltipTrigger>
        <TooltipContent side="bottom" className="items-start px-3 py-2">
          <ul className="m-0 grid list-none grid-cols-[auto_auto_minmax(0,1fr)] items-center gap-x-2 gap-y-1 p-0">
            {checks.map((c) => (
              <li key={c.id} className="contents">
                <span className={cn("size-[7px] rounded-[1.5px]", statusDot[c.status])} />
                <span className="font-mono">{c.id}</span>
                <span className="opacity-80">{c.name}</span>
              </li>
            ))}
          </ul>
        </TooltipContent>
      </Tooltip>
      {failed.length > 0 && <span className="font-mono text-[11px] text-destructive">{failed.join(" ")}</span>}
    </div>
  );
}

// Copied and already-present blobs as shares of every blob the metadata names.
export function CopyBar({ required, copied, skipped, planned }: { required: number; copied: number; skipped: number; planned?: boolean }) {
  const pct = (n: number) => `${required > 0 ? Math.min(100, (n / required) * 100) : 0}%`;
  return (
    <div className="flex h-1.5 w-full overflow-hidden rounded-full bg-muted">
      <div className={cn("h-full bg-foreground/75", planned && "bg-foreground/35")} style={{ width: pct(copied) }} />
      <div className="h-full bg-muted-foreground/35" style={{ width: pct(skipped) }} />
    </div>
  );
}

// Every recorded migrate-storage run kept in this object store, newest first.
// A row opens the full report.
export function MigrationHistory() {
  const { t, language } = useTranslation();
  const { date, time } = useMemo(() => formatters(language), [language]);
  const [openId, setOpenId] = useState<string | null>(null);
  const query = useQuery({
    ...openApiQueryOptions.listStorageMigrations(),
    meta: { suppressGlobalErrorToast: true },
  });
  const items = query.data?.items ?? [];

  return (
    <section data-testid="panel-migrations">
      <h2 className="mb-1 text-base font-semibold">{t("storage.migrations")}</h2>
      <p className="mb-3 text-sm text-muted-foreground">{t("storage.migrations-description")}</p>
      {query.error && <Alert className="mb-3">{getErrorMessageIfAny(query.error)}</Alert>}
      {items.length === 0 ? (
        <div className="rounded-md border border-dashed border-[var(--fx-border-subtle)] px-3 py-6 text-center text-sm text-muted-foreground">
          {query.isPending ? t("common.loading") : t("storage.migrations-empty")}
        </div>
      ) : (
        <TableWrap>
          <Table className="w-full min-w-[960px] table-fixed">
            <TableHeader>
              <TableRow>
                <TableHead className="w-[118px]">{t("storage.migration-finished")}</TableHead>
                <TableHead className="w-[108px]">{t("storage.migration-outcome")}</TableHead>
                <TableHead>{t("storage.migration-source")}</TableHead>
                <TableHead className="w-6" />
                <TableHead>{t("storage.migration-target")}</TableHead>
                <TableHead className="w-[132px]">{t("storage.migration-copied")}</TableHead>
                <TableHead className="w-[76px] text-right">{t("storage.migration-duration")}</TableHead>
                <TableHead className="w-[190px]">{t("storage.migration-checks")}</TableHead>
                <TableHead className="w-7" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {items.map((m) => {
                const ran = !(m.outcome === "failed" && m.failed_stage === "preflight");
                const finished = new Date(m.finished_at);
                return (
                  <TableRow
                    key={m.id}
                    className="cursor-pointer"
                    data-testid={`migration-row-${m.id}`}
                    onClick={() => setOpenId(m.id)}
                  >
                    <TableCell className="tabular-nums">
                      <div className="whitespace-nowrap">{date.format(finished)}</div>
                      <div className="whitespace-nowrap text-xs text-muted-foreground">{time.format(finished)}</div>
                    </TableCell>
                    <TableCell>
                      <OutcomeBadge outcome={m.outcome} />
                      {m.failed_stage && (
                        <div className="mt-1 whitespace-nowrap text-xs text-muted-foreground">
                          {t("storage.migration-at-stage")} {m.failed_stage}
                        </div>
                      )}
                    </TableCell>
                    <TableCell>
                      <Location location={m.source} />
                    </TableCell>
                    <TableCell className="px-0 text-muted-foreground">
                      <ArrowRight className="size-3.5" />
                    </TableCell>
                    <TableCell>
                      <Location location={m.target} />
                    </TableCell>
                    <TableCell>
                      {ran ? (
                        <>
                          <div className="mb-1 flex items-baseline justify-between gap-2 text-xs tabular-nums">
                            <span>
                              <span className="font-medium">{m.copied.toLocaleString()}</span>
                              <span className="text-muted-foreground"> / {m.required.toLocaleString()}</span>
                            </span>
                            <span className="text-muted-foreground">{formatFileSize(m.bytes_copied)}</span>
                          </div>
                          <CopyBar required={m.required} copied={m.copied} skipped={m.skipped} planned={m.outcome === "dry_run"} />
                        </>
                      ) : (
                        <span className="text-xs text-muted-foreground">{t("storage.migration-not-started")}</span>
                      )}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{formatMilliseconds(m.duration_ms)}</TableCell>
                    <TableCell className="space-y-1">
                      <CheckDots label="PF" checks={m.preflight} />
                      {m.postflight.length > 0 && <CheckDots label="PV" checks={m.postflight} />}
                    </TableCell>
                    <TableCell className="px-0 text-muted-foreground">
                      <ChevronRight className="size-4" />
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </TableWrap>
      )}
      {openId && <MigrationDetail id={openId} onClose={() => setOpenId(null)} />}
    </section>
  );
}
