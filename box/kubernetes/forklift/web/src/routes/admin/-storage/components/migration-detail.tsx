import { useEffect, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowRight, Check, Minus, X } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { CodeView } from "@/components/app-ui/code-view";
import { CopyIconButton, CopyOnHover } from "@/components/app-ui/copy-button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { CopyBar, OutcomeBadge, formatters } from "@/routes/admin/-storage/components/migration-history";
import { Location, ProviderLogo, providerName } from "@/routes/admin/-storage/components/provider-logo";
import { StatTile } from "@/routes/admin/-storage/components/stat-tile";
import { formatMilliseconds } from "@/utils/format-duration";
import { formatFileSize } from "@/utils/format-file-size";

import type { MigrationCheck, MigrationRecord, MigrationStoredAt } from "@/services/v1/openapi-types";

const statusVariant = {
  pass: "success",
  warn: "warning",
  fail: "destructive",
  skip: "default",
} as const;

const stages = ["preflight", "copy", "verify", "upload", "postflight"] as const;
type Stage = (typeof stages)[number];
type StageState = "done" | "failed" | "not-run" | "dry-run";

function stageState(m: MigrationRecord, stage: Stage): StageState {
  if (m.outcome === "succeeded") return "done";
  if (m.outcome === "dry_run") return stage === "preflight" ? "done" : "dry-run";
  const failedAt = stages.indexOf(m.failed_stage ?? "preflight");
  const at = stages.indexOf(stage);
  return at < failedAt ? "done" : at === failedAt ? "failed" : "not-run";
}

function passed(checks: MigrationCheck[]) {
  return `${checks.filter((c) => c.status === "pass").length}/${checks.length}`;
}

function stageDetail(m: MigrationRecord, stage: Stage) {
  switch (stage) {
    case "preflight":
      return passed(m.preflight);
    case "copy":
      return `${m.copied.toLocaleString()} · ${formatFileSize(m.bytes_copied)}`;
    case "verify":
      return `${m.required.toLocaleString()} blobs`;
    case "upload":
      return "meta/forklift.db";
    case "postflight":
      return m.postflight.length ? passed(m.postflight) : "";
  }
}

const reachedLine = "bg-[color-mix(in_oklch,var(--fx-success)_55%,transparent)]";

const stageStyle: Record<StageState, { ring: string; icon: ReactNode }> = {
  done: {
    ring: "border-[color-mix(in_oklch,var(--fx-success)_60%,transparent)] bg-[color-mix(in_oklch,var(--fx-success)_14%,transparent)] text-[var(--fx-success)]",
    icon: <Check className="size-3.5" />,
  },
  failed: {
    ring: "border-destructive/70 bg-destructive/15 text-destructive",
    icon: <X className="size-3.5" />,
  },
  "not-run": {
    ring: "border-border bg-muted text-muted-foreground",
    icon: <Minus className="size-3.5" />,
  },
  "dry-run": {
    ring: "border-dashed border-border text-muted-foreground",
    icon: <Minus className="size-3.5" />,
  },
};

// The five stages a run passes through, coloured by how far this one got.
function StagePipeline({ m }: { m: MigrationRecord }) {
  const { t } = useTranslation();
  return (
    <ol className="m-0 grid list-none grid-cols-5 p-0" data-testid="migration-stages">
      {stages.map((stage, i) => {
        const state = stageState(m, stage);
        const s = stageStyle[state];
        const detail =
          state === "not-run"
            ? t("storage.migration-stage-not-run")
            : state === "dry-run"
              ? t("storage.migration-stage-dry-run")
              : stageDetail(m, stage);
        return (
          <li key={stage} className="relative flex flex-col items-center text-center" data-state={state}>
            {i > 0 && (
              <span
                className={cn(
                  "absolute top-3.5 right-[calc(50%+18px)] left-[calc(-50%+18px)] h-px",
                  state === "done" || state === "failed" ? reachedLine : "bg-border",
                )}
                aria-hidden="true"
              />
            )}
            <span className="rounded-full bg-card">
              <span className={cn("grid size-7 place-items-center rounded-full border", s.ring)}>{s.icon}</span>
            </span>
            <span className={cn("mt-2 text-xs font-medium", state === "failed" && "text-destructive")}>
              {t(`storage.migration-stage-${stage}` as MessageKey)}
            </span>
            <span className="mt-0.5 px-1 text-[11px] leading-4 tabular-nums text-muted-foreground">{detail}</span>
          </li>
        );
      })}
    </ol>
  );
}

function latency(us?: number) {
  if (us === undefined) return "";
  return us < 10_000 ? `${(us / 1000).toFixed(1)}ms` : formatMilliseconds(Math.round(us / 1000));
}

function Checks({ title, summary, checks }: { title: string; summary: string; checks: MigrationCheck[] }) {
  return (
    <section className="mt-5" data-testid={`migration-checks-${title.toLowerCase()}`}>
      <div className="mb-2 flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="m-0 text-sm font-semibold">{title}</h3>
        <span className="text-xs text-muted-foreground">{summary}</span>
      </div>
      <TableWrap>
        <Table className="w-full">
          <TableHeader>
            <TableRow>
              <TableHead className="w-16">ID</TableHead>
              <TableHead className="w-52">Check</TableHead>
              <TableHead className="w-20">Status</TableHead>
              <TableHead>Detail</TableHead>
              <TableHead className="w-20 text-right">Time</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {checks.map((c) => (
              <TableRow key={c.id} className={c.status === "fail" ? "bg-destructive/10" : undefined}>
                <TableCell className="font-mono text-xs">{c.id}</TableCell>
                <TableCell className="font-mono text-xs">{c.name}</TableCell>
                <TableCell>
                  <Badge variant={statusVariant[c.status]}>{c.status.toUpperCase()}</Badge>
                </TableCell>
                <TableCell className="whitespace-normal break-words text-xs">{c.detail}</TableCell>
                <TableCell className="whitespace-nowrap text-right text-xs tabular-nums text-muted-foreground">{latency(c.latency_us)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </TableWrap>
    </section>
  );
}

function StoredAt({ at }: { at: MigrationStoredAt }) {
  const { t } = useTranslation();
  const rows: [MessageKey, string][] = [
    ["storage.migration-endpoint", at.endpoint || "s3.amazonaws.com"],
    ["storage.migration-region", at.region ?? ""],
    ["storage.migration-bucket", at.bucket],
    ["storage.migration-key", at.key],
  ];
  return (
    <section className="mt-5" data-testid="migration-stored-at">
      <h3 className="m-0 text-sm font-semibold">{t("storage.migration-stored-at")}</h3>
      <p className="mt-0.5 mb-2 text-xs text-muted-foreground">{t("storage.migration-stored-at-description")}</p>
      <div className="rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel)]">
        <div className="flex items-center gap-3 border-b border-[var(--fx-border-subtle)] px-3 py-2.5">
          <ProviderLogo id={at.provider} className="size-8" />
          <div className="min-w-0 flex-1">
            <div className="text-[13px] font-medium">{providerName(at.provider)}</div>
            <div className="flex min-w-0 items-center gap-1">
              <span className="truncate font-mono text-xs text-muted-foreground">{at.uri}</span>
              <CopyIconButton value={at.uri} />
            </div>
          </div>
        </div>
        <dl className="m-0 grid grid-cols-[120px_minmax(0,1fr)] items-center gap-x-3 px-3 py-2 text-xs">
          {rows
            .filter(([, v]) => v)
            .map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="text-muted-foreground">{t(k)}</dt>
                <dd className="m-0 flex min-h-7 min-w-0 items-center">
                  <CopyOnHover value={v}>
                    <span className="truncate font-mono">{v}</span>
                  </CopyOnHover>
                </dd>
              </div>
            ))}
        </dl>
      </div>
    </section>
  );
}

function Report({ m }: { m: MigrationRecord }) {
  const { t, language } = useTranslation();
  const { full } = formatters(language);
  const failedChecks = [...m.preflight, ...m.postflight].filter((c) => c.status === "fail");
  const copyRan = !(m.outcome === "failed" && m.failed_stage === "preflight");
  const count = (n: number) => (copyRan ? n.toLocaleString() : "-");
  const when = (iso: string) => <span className="block text-[13px] leading-snug font-normal">{full.format(new Date(iso))}</span>;

  return (
    <>
      <div className="rounded-md border border-[var(--fx-border-subtle)] px-3 pt-4 pb-3">
        <StagePipeline m={m} />
      </div>

      {m.outcome === "failed" && (
        <Alert className="mt-4" data-testid="migration-failure">
          <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
            <span className="font-semibold">
              {t("storage.migration-failed-at")} {m.failed_stage}
            </span>
            {m.error && <span className="text-xs text-muted-foreground">{m.error}</span>}
          </div>
          {failedChecks.length > 0 && (
            <ul className="m-0 mt-2 list-none space-y-1.5 p-0">
              {failedChecks.map((c) => (
                <li key={c.id} className="grid grid-cols-[auto_minmax(0,1fr)] items-baseline gap-2 text-xs">
                  <Badge variant="destructive" className="font-mono">
                    {c.id}
                  </Badge>
                  <span className="min-w-0 break-words">
                    <span className="font-mono">{c.name}</span>
                    <span className="text-muted-foreground">: {c.detail}</span>
                  </span>
                </li>
              ))}
            </ul>
          )}
          {m.remediation && (
            <div className="mt-2.5 border-t border-destructive/25 pt-2 text-xs">
              <span className="font-semibold">{t("storage.migration-remediation")}</span>
              <span className="text-muted-foreground">: {m.remediation}</span>
            </div>
          )}
        </Alert>
      )}

      <div className="mt-4 grid grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-3">
        {[m.source, m.target].map((loc, i) => (
          <div key={i} className="contents">
            {i === 1 && <ArrowRight className="size-4 text-muted-foreground" />}
            <div className="min-w-0 rounded-md border border-[var(--fx-border-subtle)] px-3 py-2.5">
              <div className="mb-1.5 text-[11px] font-medium tracking-wide text-muted-foreground uppercase">
                {t(i === 0 ? "storage.migration-source" : "storage.migration-target")}
              </div>
              <Location location={loc} />
            </div>
          </div>
        ))}
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile label={t("storage.migration-started")} value={when(m.started_at)} />
        <StatTile label={t("storage.migration-finished")} value={when(m.finished_at)} />
        <StatTile label={t("storage.migration-duration")} value={formatMilliseconds(m.duration_ms)} hint={`forklift ${m.forklift_version}`} />
        <StatTile
          label={t("storage.migration-metadata")}
          value={m.meta_copied ? t("storage.migration-meta-copied") : t("storage.migration-meta-not-copied")}
        />
        <StatTile
          label={t("storage.migration-copied")}
          value={count(m.copied)}
          hint={
            copyRan ? (
              <span className="block">
                <span className="mb-1.5 block">
                  {formatFileSize(m.bytes_copied)}, {m.skipped.toLocaleString()} {t("storage.migration-skipped")}
                </span>
                <CopyBar required={m.required} copied={m.copied} skipped={m.skipped} planned={m.outcome === "dry_run"} />
              </span>
            ) : undefined
          }
        />
        <StatTile label={t("storage.migration-required")} value={count(m.required)} hint={t("storage.migration-required-hint")} />
        <StatTile label={t("storage.migration-verified")} value={count(m.verified_blobs)} hint={`verify ${m.settings.verify}`} />
        <StatTile
          label={t("storage.migration-settings")}
          value={<span className="text-sm font-normal">concurrency {m.settings.concurrency}</span>}
          hint={
            [
              m.settings.dry_run && "dry-run",
              m.settings.overwrite_meta && "overwrite-meta",
              m.settings.allow_missing_source_blobs && "allow-missing-source-blobs",
              m.settings.require_conditional_writes && "require-conditional-writes",
            ]
              .filter(Boolean)
              .join(", ") || "-"
          }
        />
      </div>

      <Checks title="Preflight" summary={m.preflight_summary} checks={m.preflight} />
      {m.postflight.length > 0 && <Checks title="Postflight" summary={m.postflight_summary ?? ""} checks={m.postflight} />}
      {m.stored_at && <StoredAt at={m.stored_at} />}
    </>
  );
}

function RawJson({ m }: { m: MigrationRecord }) {
  const { t } = useTranslation();
  const { stored_at: _, ...stored } = m;
  const json = JSON.stringify(stored, null, 2);
  return (
    <div data-testid="migration-json">
      <p className="m-0 mb-2 truncate text-xs text-muted-foreground">
        {t("storage.migration-json-hint")} {m.stored_at && <span className="font-mono">{m.stored_at.uri}</span>}
      </p>
      <div className="relative">
        <CodeView code={json} language="json" lineNumbers />
        <CopyIconButton
          value={json}
          className="absolute top-2 right-3 bg-card/80 opacity-100 backdrop-blur-sm"
        />
      </div>
    </div>
  );
}

// The full report of one migration run: how far it got, what was copied and
// verified, why it failed and what was undone, every check, and where the
// record itself is stored. The JSON tab shows the stored object as is.
export function MigrationDetail({ id, onClose }: { id: string; onClose: () => void }) {
  const { t } = useTranslation();
  const query = useQuery({
    ...openApiQueryOptions.getStorageMigration({ path: { id } }),
    meta: { suppressGlobalErrorToast: true },
  });
  const m = query.data;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-100 flex items-start justify-center overflow-y-auto overscroll-y-none bg-black/70 py-10 backdrop-blur-[3px]"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("storage.migration-report")}
        data-testid="migration-detail"
        className="w-[960px] max-w-[94vw] rounded-lg border border-border bg-card p-5 shadow-[var(--fx-overlay-shadow)]"
        onClick={(e) => e.stopPropagation()}
      >
        <Tabs defaultValue="report">
          <div className="mb-4 flex items-center justify-between gap-3">
            <div className="min-w-0">
              <h2 className="m-0 flex flex-wrap items-center gap-2 text-base leading-6 font-semibold">
                {t("storage.migration-report")}
                {m && <OutcomeBadge outcome={m.outcome} />}
              </h2>
              <CopyOnHover value={id}>
                <span className="font-mono text-xs leading-5 text-muted-foreground">{id}</span>
              </CopyOnHover>
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              <TabsList className="h-8">
                <TabsTrigger value="report">{t("storage.migration-view-report")}</TabsTrigger>
                <TabsTrigger value="json" data-testid="migration-view-json">
                  {t("storage.migration-view-json")}
                </TabsTrigger>
              </TabsList>
              <Button variant="ghost" size="icon-sm" onClick={onClose} aria-label={t("common.close")}>
                <X />
              </Button>
            </div>
          </div>

          {query.error && <Alert>{getErrorMessageIfAny(query.error)}</Alert>}
          {!m ? (
            !query.error && <div className="text-sm text-muted-foreground">{t("common.loading")}</div>
          ) : (
            <>
              <TabsContent value="report">
                <Report m={m} />
              </TabsContent>
              <TabsContent value="json">
                <RawJson m={m} />
              </TabsContent>
            </>
          )}
        </Tabs>
      </div>
    </div>
  );
}
