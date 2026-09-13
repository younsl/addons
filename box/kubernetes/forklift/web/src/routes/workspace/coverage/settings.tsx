import { useEffect, useMemo, useState } from "react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, CheckCircle2, Globe, Send, XCircle } from "lucide-react";
import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Switch } from "@/components/ui/switch";
import { openApiQueryKeys } from "@/query/v1/openapi-query-keys";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from "@/components/ui/combobox";
import {
  postCheckHostCoverageSettings,
  postSendCoverageNotification,
  updateCoverageSettings,
} from "@/services/v1/coverage/api";
import type { CoverageSettings, CoverageSettingsInput } from "@/services/v1/openapi-types";
import { useDateTime, useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatMilliseconds } from "@/utils/format-duration";
import { validateExternalDomain, type HostProblem } from "@/lib/coverage-host";
import { Redirect } from "@/components/app/redirect";

export const Route = createFileRoute("/workspace/coverage/settings")({
  component: CoverageSettingsRoute,
});

// Settings live under Coverage rather than in the Admin section: they are only
// meaningful next to the number they shape, and everyone who reads that number
// arrives here. Non-admins are sent back to the dashboard, which is the whole
// surface they have.
function CoverageSettingsRoute() {
  const { me } = useAuth();
  return me.admin ? <CoverageSettingsPage /> : <Redirect to="/workspace/coverage" replace />;
}

// Form mirrors the settings, with the list fields held as the comma-separated
// text the inputs actually edit. They are split on save rather than on every
// keystroke, so typing a comma does not reorder what is on screen.
interface Form {
  forklift_host: string;
  exclude_topics: string;
  scan_cron: string;
  timezone: string;
  auto_scan_enabled: boolean;
  report_enabled: boolean;
  receiver: string;
  skip_when_full_coverage: boolean;
  max_branches: number;
  since_days: number;
  use_search: boolean;
}

// COMMON_TIMEZONES is a shortlist, not the IANA database. It covers the zones a
// team is most likely to schedule against; anything else is typed straight into
// the same field, and the server validates it either way.
//
// The list is filtered against what has been typed rather than shown whole: a
// name like Asia/Seoul is reached by typing "seo", and an unfiltered list would
// make the field behave like a dropdown that ignores the keyboard.
const COMMON_TIMEZONES = [
  "UTC",
  "Asia/Seoul",
  "Asia/Tokyo",
  "Asia/Shanghai",
  "Asia/Singapore",
  "Asia/Kolkata",
  "Europe/London",
  "Europe/Berlin",
  "Europe/Paris",
  "America/New_York",
  "America/Chicago",
  "America/Los_Angeles",
  "Australia/Sydney",
];

// formFromSettings maps the stored settings onto the fields the form edits.
// The list field is held as the comma-separated text the input works with and
// split on save, so typing a comma does not reorder what is on screen.
function formFromSettings(settings: CoverageSettings): Form {
  return {
    forklift_host: settings.forklift_host,
    exclude_topics: settings.exclude_topics.join(", "),
    scan_cron: settings.scan_cron,
    timezone: settings.timezone,
    auto_scan_enabled: settings.auto_scan_enabled,
    report_enabled: settings.report_enabled,
    receiver: settings.receiver,
    skip_when_full_coverage: settings.skip_when_full_coverage,
    max_branches: settings.max_branches,
    since_days: settings.since_days,
    use_search: settings.use_search,
  };
}

function splitList(value: string): string[] {
  return value
    .split(",")
    .map((entry) => entry.trim())
    .filter(Boolean);
}

// Each rejection names its own reason, so the field says what is wrong with the
// entry rather than only that something is.
// The four states a check can be in, and the one place their colour is decided.
type Tone = "ok" | "bad" | "warn" | "idle";

const TONE_CLASS: Record<Tone, string> = {
  ok: "text-[var(--fx-success)]",
  bad: "text-[var(--fx-danger)]",
  warn: "text-[var(--fx-warning)]",
  idle: "text-muted-foreground",
};

function StatusIcon({ tone, className }: { tone: Tone; className?: string }) {
  const shared = cn("size-3.5", TONE_CLASS[tone], className);
  if (tone === "ok") return <CheckCircle2 className={shared} aria-hidden="true" />;
  if (tone === "bad") return <XCircle className={shared} aria-hidden="true" />;
  if (tone === "warn") return <AlertTriangle className={shared} aria-hidden="true" />;
  return <Globe className={shared} aria-hidden="true" />;
}

const HOST_PROBLEM_KEY: Record<HostProblem, MessageKey> = {
  required: "coverage.host-required",
  "not-bare": "coverage.host-not-bare",
  "port-range": "coverage.host-port-range",
  "too-long": "coverage.host-too-long",
  "not-external": "coverage.host-not-external",
};

function CoverageSettingsPage() {
  const { t } = useTranslation();
  const formatDateTime = useDateTime();
  const queryClient = useQueryClient();
  const [form, setForm] = useState<Form | null>(null);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);

  const { data: settings, error: loadError, isLoading } = useQuery({
    ...openApiQueryOptions.getCoverageSettings(),
    // Settings change only when somebody saves them, so there is nothing to
    // gain from refetching on every window focus, and something to lose: the
    // form is seeded from this query.
    staleTime: 60_000,
  });
  const { data: receivers = [] } = useQuery(openApiQueryOptions.listNotificationReceivers());
  const { data: preview } = useQuery(openApiQueryOptions.getCoverageNotificationPreview());

  // Seeded once, not on every change of the query data. A background refetch
  // hands back a new object, and re-seeding on that would wipe whatever is
  // half-typed in the form. Saving re-seeds explicitly below, which is the only
  // moment the stored values should replace what is on screen.
  useEffect(() => {
    if (!settings) return;
    setForm((prev) => prev ?? formFromSettings(settings));
  }, [settings]);

  const save = useMutation({
    mutationFn: (body: CoverageSettingsInput) => updateCoverageSettings({ body }),
    onSuccess: (stored) => {
      setError("");
      setSaved(true);
      // Show what was actually stored: the server trims and normalises, so the
      // field should read back the value that will be used, not the keystrokes.
      setForm(formFromSettings(stored));
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverageSettings() });
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverage() });
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverageNotificationPreview() });
    },
    onError: (e) => {
      setSaved(false);
      setError((e as Error).message);
    },
  });

  const send = useMutation({
    mutationFn: () => postSendCoverageNotification(),
  });

  const receiverNames = useMemo(() => receivers.map((r) => r.name), [receivers]);
  // Names index the list everywhere else, so the description is looked up by
  // name rather than threaded through the form.
  const receiverDescriptions = useMemo(() => {
    const byName: Record<string, string> = {};
    for (const r of receivers) byName[r.name] = r.description;
    return byName;
  }, [receivers]);

  // Typing filters the list. Unlike the timezone field this is a closed set, so
  // a name that is not a receiver is not something to accept: the input mirrors
  // the selection rather than standing on its own.
  const receiverOptions = useMemo(() => {
    const typed = form?.receiver.trim().toLowerCase() ?? "";
    if (!typed) return receiverNames;
    return receiverNames.filter((name) => name.toLowerCase().includes(typed));
  }, [receiverNames, form?.receiver]);

  // The syntax verdict is local so the field answers on every keystroke; the
  // server would only repeat it a round trip later.
  const typedHost = form?.forklift_host.trim() ?? "";
  const hostProblem = typedHost === "" ? null : validateExternalDomain(typedHost);
  const hasInvalidHost = hostProblem !== null;

  // DNS is the server's to answer, and only worth asking once the name could be
  // a host at all. Debounced so a lookup does not fire per keystroke.
  const [dnsQueryHost, setDnsQueryHost] = useState("");
  useEffect(() => {
    if (hasInvalidHost || typedHost === "") {
      setDnsQueryHost("");
      return;
    }
    const id = setTimeout(() => setDnsQueryHost(typedHost), 500);
    return () => clearTimeout(id);
  }, [typedHost, hasInvalidHost]);

  const { data: dnsCheck, isFetching: dnsChecking } = useQuery({
    queryKey: ["coverage-host-check", dnsQueryHost],
    queryFn: () => postCheckHostCoverageSettings({ body: { forklift_host: dnsQueryHost } }),
    enabled: dnsQueryHost !== "",
    staleTime: 60_000,
    retry: false,
  });

  // Required means "the scan cannot run without it". The host is only required
  // when there is no deployment value behind it to fall back to, which is why
  // that default is reported separately from the effective value.
  const missingRequired = (() => {
    if (!form || !settings) return true;
    if (typedHost === "" && settings.forklift_host_default === "") return true;
    if (form.scan_cron.trim() === "" || form.timezone.trim() === "") return true;
    if (!Number.isInteger(form.max_branches) || !Number.isInteger(form.since_days)) return true;
    return false;
  })();
  const saveBlocked = hasInvalidHost || missingRequired;

  // Matched on any part of the name and case-insensitively, so "seoul" and
  // "asia" both find Asia/Seoul. An empty field offers the whole shortlist.
  const timezoneOptions = (() => {
    const typed = form?.timezone.trim().toLowerCase() ?? "";
    if (!typed) return COMMON_TIMEZONES;
    return COMMON_TIMEZONES.filter((zone) => zone.toLowerCase().includes(typed));
  })();

  // The one line under the host field. Syntax first, because a name that cannot
  // be a host has nothing to resolve; then the lookup, which is advisory.
  const hostStatus: { tone: Tone; message: string } = (() => {
    if (typedHost === "") return { tone: "idle", message: t("coverage.host-defaulted") };
    if (hostProblem !== null) return { tone: "bad", message: t(HOST_PROBLEM_KEY[hostProblem]) };
    if (dnsChecking || dnsQueryHost !== typedHost) {
      return { tone: "idle", message: t("coverage.host-checking") };
    }
    // Outcome and how long it took, nothing else. The addresses were noise: they
    // are not what the field is being checked for, and a name behind a pool
    // pushed the answer off the line. The time stays because a lookup that takes
    // 900ms is working and still worth noticing.
    if (dnsCheck?.resolved) {
      return {
        tone: "ok",
        message: `${t("coverage.host-resolves")} (${formatMilliseconds(dnsCheck.latency_ms)})`,
      };
    }
    if (dnsCheck) {
      // Not an error: forklift resolves from inside the cluster and the builds
      // this measures resolve from wherever they run.
      return {
        tone: "warn",
        message: `${t("coverage.host-no-dns")} (${formatMilliseconds(dnsCheck.latency_ms)})`,
      };
    }
    return { tone: "idle", message: t("coverage.host-checking") };
  })();

  if (isLoading) return <div className="p-4 text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (loadError || !settings || !form) {
    return (
      <>
        <PageHeader title={t("coverage.settings")} />
        <Alert>{t("coverage.unavailable")}</Alert>
      </>
    );
  }

  const set = <K extends keyof Form>(key: K, value: Form[K]) => {
    setSaved(false);
    setForm((prev) => (prev ? { ...prev, [key]: value } : prev));
  };
  // The server validates the expression whether or not the schedule is on, so a
  // field left empty would block saving from behind a disabled input. Turning
  // the schedule off restores whatever was last stored.
  const setAutoScan = (enabled: boolean) => {
    setSaved(false);
    setForm((prev) =>
      prev
        ? {
            ...prev,
            auto_scan_enabled: enabled,
            scan_cron: enabled || prev.scan_cron.trim() !== "" ? prev.scan_cron : settings.scan_cron,
            timezone: enabled || prev.timezone.trim() !== "" ? prev.timezone : settings.timezone,
          }
        : prev
    );
  };

  const num = (key: keyof Form) => (value: string) => {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) set(key, parsed as Form[typeof key]);
  };

  const onSave = () =>
    save.mutate({
      forklift_host: form.forklift_host,
      exclude_topics: splitList(form.exclude_topics),
      scan_cron: form.scan_cron,
      timezone: form.timezone,
      auto_scan_enabled: form.auto_scan_enabled,
      report_enabled: form.report_enabled,
      receiver: form.receiver,
      skip_when_full_coverage: form.skip_when_full_coverage,
      max_branches: form.max_branches,
      since_days: form.since_days,
      use_search: form.use_search,
    });

  return (
    <>
      <PageHeader
        title={t("coverage.settings")}
        actions={
          <>
            <Link to="/workspace/coverage" className={buttonVariants({ variant: "outline" })}>
              {t("coverage.title")}
            </Link>
            <Button
              onClick={onSave}
              disabled={save.isPending || saveBlocked}
              title={saveBlocked ? t("coverage.fill-required") : undefined}
            >
              {t("common.save")}
            </Button>
          </>
        }
      />
      <PageDescription>{t("coverage.settings-description")}</PageDescription>

      {error && <Alert className="mb-4">{error}</Alert>}
      {saved && (
        <div className="mb-4 rounded-md border border-[var(--fx-success)]/50 bg-[var(--fx-success)]/10 px-3 py-2 text-sm">
          {t("coverage.settings-saved")}
        </div>
      )}

      <Card className="mb-4">
        <CardContent className="pt-4">
          <h2 className="mb-1 text-sm font-medium">{t("coverage.scope")}</h2>
          <p className="mb-3 text-sm text-muted-foreground">{t("coverage.scope-hint")}</p>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="cov-host">{t("coverage.forklift-host")}</FieldLabel>
              {/* The status icon leads the field, so the verdict sits where the
                  eye already is rather than below the control. */}
              <InputGroup>
                <InputGroupAddon>
                  <StatusIcon tone={hostStatus.tone} />
                </InputGroupAddon>
                <InputGroupInput
                  id="cov-host"
                  value={form.forklift_host}
                  placeholder={settings.forklift_host || "forklift.example.com"}
                  aria-invalid={hasInvalidHost || undefined}
                  onChange={(e) => set("forklift_host", e.target.value)}
                />
              </InputGroup>
              <FieldDescription>{t("coverage.forklift-host-hint")}</FieldDescription>
              <p className={cn("flex min-w-0 items-start gap-1.5 text-[13px]", TONE_CLASS[hostStatus.tone])}>
                <StatusIcon tone={hostStatus.tone} className="mt-0.5 shrink-0" />
                <span className="min-w-0 break-all">{hostStatus.message}</span>
              </p>
            </Field>

            <Field>
              <FieldLabel htmlFor="cov-topics">{t("coverage.exclude-topics")}</FieldLabel>
              <Input id="cov-topics" value={form.exclude_topics} onChange={(e) => set("exclude_topics", e.target.value)} />
              <FieldDescription>{t("coverage.exclude-topics-hint")}</FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="cov-branches">{t("coverage.max-branches")}</FieldLabel>
              <Input
                id="cov-branches"
                type="number"
                value={form.max_branches}
                onChange={(e) => num("max_branches")(e.target.value)}
              />
              <FieldDescription>{t("coverage.max-branches-hint")}</FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="cov-since">{t("coverage.since-days")}</FieldLabel>
              <Input
                id="cov-since"
                type="number"
                value={form.since_days}
                onChange={(e) => num("since_days")(e.target.value)}
              />
              <FieldDescription>{t("coverage.since-days-hint")}</FieldDescription>
            </Field>
            <Field orientation="horizontal">
              <Switch id="cov-search" checked={form.use_search} onCheckedChange={(v) => set("use_search", Boolean(v))} />
              <FieldLabel htmlFor="cov-search">{t("coverage.use-search")}</FieldLabel>
            </Field>
            <FieldDescription>{t("coverage.use-search-hint")}</FieldDescription>
          </FieldGroup>
        </CardContent>
      </Card>

      <Card className="mb-4">
        <CardContent className="pt-4">
          <h2 className="mb-3 text-sm font-medium">{t("coverage.schedule")}</h2>
          <FieldGroup>
            <Field orientation="horizontal">
              <Switch
                id="cov-auto"
                checked={form.auto_scan_enabled}
                onCheckedChange={(v) => setAutoScan(Boolean(v))}
              />
              <FieldLabel htmlFor="cov-auto">{t("coverage.auto-scan")}</FieldLabel>
            </Field>
            {/* Cron and timezone are one decision, so they sit on one row: the
                expression is meaningless without the zone it is read in. They
                stay on screen when the schedule is off rather than disappearing,
                because the saved schedule is still worth reading, but they fade
                and stop taking input so it is clear they are not in effect. */}
            <div
              className={cn(
                "grid gap-4 transition-opacity duration-200 sm:grid-cols-2",
                !form.auto_scan_enabled && "pointer-events-none opacity-50"
              )}
              aria-disabled={!form.auto_scan_enabled || undefined}
            >
              <Field>
                <FieldLabel htmlFor="cov-cron">{t("coverage.cron")}</FieldLabel>
                <Input
                  id="cov-cron"
                  value={form.scan_cron}
                  disabled={!form.auto_scan_enabled}
                  onChange={(e) => set("scan_cron", e.target.value)}
                />
                <FieldDescription>{t("coverage.cron-hint")}</FieldDescription>
              </Field>
              <Field>
                <FieldLabel htmlFor="cov-tz">{t("coverage.timezone")}</FieldLabel>
                <Combobox
                  disabled={!form.auto_scan_enabled}
                  items={timezoneOptions}
                  inputValue={form.timezone}
                  value={COMMON_TIMEZONES.includes(form.timezone) ? form.timezone : null}
                  onInputValueChange={(next) => set("timezone", next)}
                  onValueChange={(next) => {
                    if (typeof next === "string") set("timezone", next);
                  }}
                >
                  <ComboboxInput id="cov-tz" placeholder="Asia/Seoul" className="w-full" />
                  <ComboboxContent>
                    <ComboboxEmpty>{t("coverage.timezone-freeform")}</ComboboxEmpty>
                    <ComboboxList>
                      {timezoneOptions.map((zone) => (
                        <ComboboxItem key={zone} value={zone}>
                          {zone}
                        </ComboboxItem>
                      ))}
                    </ComboboxList>
                  </ComboboxContent>
                </Combobox>
                <FieldDescription>{t("coverage.timezone-hint")}</FieldDescription>
              </Field>
            </div>
            {settings.next_run_at && (
              <p className="text-sm text-muted-foreground">
                {t("coverage.next-run")}: {formatDateTime(settings.next_run_at)}
              </p>
            )}
          </FieldGroup>
        </CardContent>
      </Card>

      <Card className="mb-4">
        <CardContent className="pt-4">
          <h2 className="mb-1 text-sm font-medium">{t("coverage.alarm")}</h2>
          <p className="mb-3 text-sm text-muted-foreground">{t("coverage.alarm-hint")}</p>
          <FieldGroup>
            <Field orientation="horizontal">
              <Switch
                id="cov-report"
                checked={form.report_enabled}
                onCheckedChange={(v) => set("report_enabled", Boolean(v))}
              />
              <FieldLabel htmlFor="cov-report">{t("coverage.send-report")}</FieldLabel>
            </Field>
            {/* Same shape as the schedule: the settings stay on screen when the
                report is off, because a receiver worth keeping is worth reading,
                but they fade and stop taking input so it is clear nothing is
                being sent. */}
            <div
              className={cn(
                "flex flex-col gap-6 transition-opacity duration-200",
                !form.report_enabled && "pointer-events-none opacity-50"
              )}
              aria-disabled={!form.report_enabled || undefined}
            >
            <Field>
              <FieldLabel htmlFor="cov-receiver">{t("coverage.receiver")}</FieldLabel>
              {receiverNames.length === 0 ? (
                // The way to create one is the link under the field, which is
                // there whether or not the list is empty.
                <p className="text-sm text-muted-foreground">{t("coverage.no-receivers")}</p>
              ) : (
                <Combobox
                  disabled={!form.report_enabled}
                  items={receiverOptions}
                  inputValue={form.receiver}
                  value={receiverNames.includes(form.receiver) ? form.receiver : null}
                  onInputValueChange={(next) => set("receiver", next)}
                  onValueChange={(next) => {
                    if (typeof next === "string") set("receiver", next);
                  }}
                >
                  <ComboboxInput
                    id="cov-receiver"
                    placeholder={t("coverage.select-receiver")}
                    className="w-full"
                  />
                  <ComboboxContent>
                    <ComboboxEmpty>{t("coverage.no-receivers-found")}</ComboboxEmpty>
                    <ComboboxList>
                      {/* The name alone does not say what a receiver is for, so
                          the description rides under it. Receivers without one
                          render as a single line rather than a blank second. */}
                      {receiverOptions.map((name) => (
                        <ComboboxItem key={name} value={name}>
                          <span className="flex min-w-0 flex-col">
                            <span className="truncate">{name}</span>
                            {receiverDescriptions[name] && (
                              <span className="truncate text-xs text-muted-foreground">
                                {receiverDescriptions[name]}
                              </span>
                            )}
                          </span>
                        </ComboboxItem>
                      ))}
                    </ComboboxList>
                  </ComboboxContent>
                </Combobox>
              )}
              {/* The link stays on screen whether or not receivers exist: the
                  list is edited somewhere else, and a reader who wants a
                  different receiver should not have to go looking for where. */}
              <FieldDescription>
                {t("coverage.receiver-hint")}{" "}
                <Link to="/admin/notifications" className="underline">
                  {t("coverage.manage-receivers")}
                </Link>
              </FieldDescription>
            </Field>
            <Field orientation="horizontal">
              <Switch
                id="cov-skip-full"
                disabled={!form.report_enabled}
                checked={form.skip_when_full_coverage}
                onCheckedChange={(v) => set("skip_when_full_coverage", Boolean(v))}
              />
              <FieldLabel htmlFor="cov-skip-full">{t("coverage.skip-when-full")}</FieldLabel>
            </Field>
            <FieldDescription>{t("coverage.skip-when-full-hint")}</FieldDescription>

            {preview && (
              <Field>
                <FieldLabel>{t("coverage.alarm-preview")}</FieldLabel>
                {preview.sample && (
                  <p className="text-sm text-muted-foreground">{t("coverage.alarm-preview-sample")}</p>
                )}
                <pre className="max-h-72 overflow-auto whitespace-pre-wrap rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel-raised)] p-3 text-[12px] leading-6">
                  {preview.payload.text}
                </pre>
                <div className="flex flex-wrap items-center gap-2">
                  <Button
                    variant="outline"
                    onClick={() => send.mutate()}
                    disabled={send.isPending || !form.report_enabled || form.receiver === ""}
                  >
                    <Send className="size-4" aria-hidden="true" />
                    {t("coverage.send-now")}
                  </Button>
                  {send.data?.results.map((r) => (
                    <span
                      key={r.name}
                      className={r.ok ? "text-sm text-[var(--fx-success)]" : "text-sm text-[var(--fx-danger)]"}
                    >
                      {r.name}: {r.ok ? t("coverage.sent") : r.error}
                    </span>
                  ))}
                  {send.isError && (
                    <span className="text-sm text-[var(--fx-danger)]">{(send.error as Error).message}</span>
                  )}
                </div>
              </Field>
            )}
            </div>
          </FieldGroup>
        </CardContent>
      </Card>

      {/* There is no crawl-tuning panel. How fast the scan may go is discovered
          from how GitLab responds, not configured; the log line that closes a
          scan reports the concurrency it settled on. */}
      {settings.updated_at && (
        <p className="text-sm text-muted-foreground">
          {t("coverage.last-updated")}: {formatDateTime(settings.updated_at)}
          {settings.updated_by && ` (${settings.updated_by})`}
        </p>
      )}
    </>
  );
}
