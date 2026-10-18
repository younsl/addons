import { useEffect, useState } from "react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { Send } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { AlarmPreview } from "@/components/app-ui/alarm-preview";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Select } from "@/components/app-ui/select";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useDateTime, useTranslation, type MessageKey } from "@/lib/i18n";
import { COMMON_TIMEZONES } from "@/lib/timezones";
import { cn } from "@/lib/utils";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { sendInsightsNotification, updateInsightSettings } from "@/services/v1/insights/api";
import type { InsightSettings, InsightSettingsInput } from "@/services/v1/openapi-types";
import { InsightsTabs } from "./-components/insights-tabs";

export const Route = createFileRoute("/workspace/insights/settings")({
  component: InsightSettingsPage,
});

const SEVERITIES = ["critical", "high", "medium", "low", "info"] as const;
const MIN_INTERVAL = 5;
const MAX_INTERVAL = 1440;

type Form = Omit<InsightSettings, "updated_by" | "updated_at" | "next_analysis_at" | "next_report_at">;

function formFromSettings(s: InsightSettings): Form {
  return {
    auto_analyze_enabled: s.auto_analyze_enabled,
    interval_minutes: s.interval_minutes,
    report_enabled: s.report_enabled,
    receiver: s.receiver,
    report_cron: s.report_cron,
    timezone: s.timezone,
    min_severity: s.min_severity,
    skip_when_unchanged: s.skip_when_unchanged,
  };
}

// InsightSettingsPage edits when the security analysis runs and when its alarm
// goes out. It mirrors the Coverage settings: each feature keeps its own
// schedule and receiver, while the receivers themselves live in Notifications.
function InsightSettingsPage() {
  const { t } = useTranslation();
  const formatDateTime = useDateTime();
  const queryClient = useQueryClient();
  const [form, setForm] = useState<Form | null>(null);
  const navigate = useNavigate();

  const query = useQuery({
    ...openApiQueryOptions.getInsightSettings(),
    staleTime: 60_000,
    meta: { suppressGlobalErrorToast: true },
  });
  const settings = query.data;
  const { data: receivers = [] } = useQuery(openApiQueryOptions.listNotificationReceivers());
  const preview = useQuery({
    ...openApiQueryOptions.getInsightsNotificationPreview(),
    meta: { suppressGlobalErrorToast: true },
  });
  const send = useMutation({
    mutationFn: (id: number) => sendInsightsNotification({ body: { receiver_ids: [id] } }),
  });

  // Seeded once so a background refetch does not wipe a half-typed form.
  useEffect(() => {
    if (settings) setForm((prev) => prev ?? formFromSettings(settings));
  }, [settings]);

  const save = useMutation({
    mutationFn: (body: InsightSettingsInput) => updateInsightSettings({ body }),
    // A saved settings page goes back to the Security tab it belongs to; a
    // failed save stays to show why.
    onSuccess: (stored) => {
      queryClient.setQueryData(openApiQueryKeys.getInsightSettings(), stored);
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getInsights() });
      navigate({ to: "/workspace/insights" });
    },
  });

  const header = <InsightsTabs active="security" />;
  if (query.isLoading) return <>{header}<p className="text-sm text-muted-foreground">{t("common.loading")}</p></>;
  if (!settings || !form) {
    return (
      <>
        {header}
        <PageHeader title={t("insights.settings-title")} />
        <Alert>{t("insights.settings-unavailable")}</Alert>
      </>
    );
  }

  const set = <K extends keyof Form>(key: K, value: Form[K]) => {
    setForm((prev) => (prev ? { ...prev, [key]: value } : prev));
  };
  const intervalValid = Number.isInteger(form.interval_minutes)
    && form.interval_minutes >= MIN_INTERVAL && form.interval_minutes <= MAX_INTERVAL;
  const blocked = !intervalValid || form.report_cron.trim() === "" || form.timezone.trim() === ""
    || (form.report_enabled && form.receiver === "");
  // The manual send goes to the receiver selected on screen, saved or not.
  const receiverId = preview.data?.receivers.find((r) => r.name === form.receiver)?.id;
  const timezones = (() => {
    const typed = form.timezone.trim().toLowerCase();
    return typed ? COMMON_TIMEZONES.filter((z) => z.toLowerCase().includes(typed)) : COMMON_TIMEZONES;
  })();

  return (
    <>
      {header}
      <PageHeader
        title={t("insights.settings-title")}
        actions={
          <>
            <Link to="/workspace/insights" className={buttonVariants({ variant: "outline" })}>
              {t("insights.title")}
            </Link>
            <Button onClick={() => save.mutate(form)} disabled={save.isPending || blocked}>
              {t("common.save")}
            </Button>
          </>
        }
      />
      <PageDescription>{t("insights.settings-description")}</PageDescription>

      {save.isError && <Alert className="mb-4">{getErrorMessageIfAny(save.error)}</Alert>}

      <Card className="mb-4">
        <CardContent className="pt-4">
          <h2 className="mb-3 text-sm font-medium">{t("insights.settings-analysis")}</h2>
          <FieldGroup>
            <Field orientation="horizontal">
              <Switch id="ins-auto" checked={form.auto_analyze_enabled}
                onCheckedChange={(v) => set("auto_analyze_enabled", Boolean(v))} />
              <FieldLabel htmlFor="ins-auto">{t("insights.settings-auto")}</FieldLabel>
            </Field>
            <Field className={cn("transition-opacity duration-200", !form.auto_analyze_enabled && "pointer-events-none opacity-50")}
              aria-disabled={!form.auto_analyze_enabled || undefined}>
              <FieldLabel htmlFor="ins-interval">{t("insights.settings-interval")}</FieldLabel>
              <Input id="ins-interval" type="number" min={MIN_INTERVAL} max={MAX_INTERVAL} className="sm:w-40"
                disabled={!form.auto_analyze_enabled} value={form.interval_minutes}
                aria-invalid={!intervalValid || undefined}
                onChange={(e) => set("interval_minutes", Number(e.target.value))} />
              <FieldDescription>{t("insights.settings-interval-hint")}</FieldDescription>
            </Field>
            {settings.next_analysis_at && form.auto_analyze_enabled && (
              <p className="text-sm text-muted-foreground">
                {t("insights.settings-next-analysis")}: {formatDateTime(settings.next_analysis_at)}
              </p>
            )}
          </FieldGroup>
        </CardContent>
      </Card>

      <Card className="mb-4">
        <CardContent className="pt-4">
          <h2 className="mb-1 text-sm font-medium">{t("insights.settings-alarm")}</h2>
          <p className="mb-3 text-sm text-muted-foreground">{t("insights.settings-alarm-hint")}</p>
          <FieldGroup>
            <Field orientation="horizontal">
              <Switch id="ins-report" checked={form.report_enabled}
                onCheckedChange={(v) => set("report_enabled", Boolean(v))} />
              <FieldLabel htmlFor="ins-report">{t("insights.settings-send-report")}</FieldLabel>
            </Field>
            {/* The alarm settings stay readable while it is off, faded and
                inert, the same as the Coverage report. */}
            <div className={cn("flex flex-col gap-6 transition-opacity duration-200", !form.report_enabled && "pointer-events-none opacity-50")}
              aria-disabled={!form.report_enabled || undefined}>
              <Field>
                <FieldLabel htmlFor="ins-receiver">{t("insights.settings-receiver")}</FieldLabel>
                {receivers.length === 0 ? (
                  <p className="m-0 text-sm text-muted-foreground">{t("coverage.no-receivers")}</p>
                ) : (
                  <Select className="w-full sm:w-72" value={form.receiver} disabled={!form.report_enabled}
                    placeholder={t("insights.settings-receiver-none")}
                    onChange={(v) => set("receiver", v)}
                    options={receivers.map((r) => ({ value: r.name, label: r.name, description: r.description || undefined }))} />
                )}
                <FieldDescription>
                  {t("insights.settings-receiver-hint")}{" "}
                  <Link to="/admin/notifications" className="underline">{t("coverage.manage-receivers")}</Link>
                </FieldDescription>
              </Field>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor="ins-cron">{t("coverage.cron")}</FieldLabel>
                  <Input id="ins-cron" value={form.report_cron} disabled={!form.report_enabled}
                    onChange={(e) => set("report_cron", e.target.value)} />
                  <FieldDescription>{t("coverage.cron-hint")}</FieldDescription>
                </Field>
                <Field>
                  <FieldLabel htmlFor="ins-tz">{t("coverage.timezone")}</FieldLabel>
                  <Combobox disabled={!form.report_enabled} items={timezones} inputValue={form.timezone}
                    value={COMMON_TIMEZONES.includes(form.timezone) ? form.timezone : null}
                    onInputValueChange={(next) => set("timezone", next)}
                    onValueChange={(next) => { if (typeof next === "string") set("timezone", next); }}>
                    <ComboboxInput id="ins-tz" placeholder="Asia/Seoul" className="w-full" />
                    <ComboboxContent>
                      <ComboboxEmpty>{t("coverage.timezone-freeform")}</ComboboxEmpty>
                      <ComboboxList>
                        {timezones.map((zone) => <ComboboxItem key={zone} value={zone}>{zone}</ComboboxItem>)}
                      </ComboboxList>
                    </ComboboxContent>
                  </Combobox>
                  <FieldDescription>{t("coverage.timezone-hint")}</FieldDescription>
                </Field>
              </div>
              <Field>
                <FieldLabel>{t("insights.settings-min-severity")}</FieldLabel>
                <Select className="w-full sm:w-72" value={form.min_severity} disabled={!form.report_enabled}
                  onChange={(v) => set("min_severity", v as Form["min_severity"])}
                  options={SEVERITIES.map((s) => ({ value: s, label: t(`insights.severity-${s}` as MessageKey) }))} />
                <FieldDescription>{t("insights.settings-min-severity-hint")}</FieldDescription>
              </Field>
              <div>
                <Field orientation="horizontal">
                  <Switch id="ins-skip" disabled={!form.report_enabled} checked={form.skip_when_unchanged}
                    onCheckedChange={(v) => set("skip_when_unchanged", Boolean(v))} />
                  <FieldLabel htmlFor="ins-skip">{t("insights.settings-skip-unchanged")}</FieldLabel>
                </Field>
                <FieldDescription className="mt-1.5">{t("insights.settings-skip-unchanged-hint")}</FieldDescription>
              </div>
              {settings.next_report_at && form.report_enabled && (
                <p className="m-0 text-sm text-muted-foreground">
                  {t("insights.settings-next-alarm")}: {formatDateTime(settings.next_report_at)}
                </p>
              )}
              <Field>
                <FieldLabel>{t("coverage.alarm-preview")}</FieldLabel>
                {preview.data ? (
                  <AlarmPreview text={preview.data.payload.text} />
                ) : (
                  <p className="m-0 text-sm text-muted-foreground">
                    {preview.isLoading ? t("common.loading") : t("insights.settings-preview-empty")}
                  </p>
                )}
                <div className="flex flex-wrap items-center gap-2">
                  <Button variant="outline" disabled={send.isPending || !preview.data || receiverId === undefined}
                    onClick={() => receiverId !== undefined && send.mutate(receiverId)}>
                    <Send className="size-4" aria-hidden="true" />
                    {t("coverage.send-now")}
                  </Button>
                  {send.data?.results.map((r) => (
                    <span key={r.name} className={r.ok ? "text-sm text-[var(--fx-success)]" : "text-sm text-[var(--fx-danger)]"}>
                      {r.name}: {r.ok ? t("coverage.sent") : r.error}
                    </span>
                  ))}
                  {send.isError && <span className="text-sm text-[var(--fx-danger)]">{getErrorMessageIfAny(send.error)}</span>}
                </div>
              </Field>
            </div>
          </FieldGroup>
        </CardContent>
      </Card>

      {settings.updated_at && (
        <p className="text-sm text-muted-foreground">
          {t("coverage.last-updated")}: {formatDateTime(settings.updated_at)}
          {settings.updated_by && ` (${settings.updated_by})`}
        </p>
      )}
    </>
  );
}
