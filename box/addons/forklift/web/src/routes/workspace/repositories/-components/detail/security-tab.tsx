import { ACL_MAX_SAFE, aclEntryInfo, fmtCount } from "@/routes/workspace/repositories/-utils/ip-acl";
import type { PolicySelection } from "@/routes/workspace/repositories/-components/detail/policy-flow";
import { X } from "lucide-react";
import { useState } from "react";
import { NotificationSamplePreview, api } from "@/api";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { Select } from "@/components/app-ui/select";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import type { Repository } from "@/services/v1/openapi-types";
import type { RepositoryDraftSetter } from "@/routes/workspace/repositories/-hooks/use-repository-draft";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { useReceiversList } from "@/routes/admin/notifications/-hooks/use-receiver-mutations";
import { useRepositoriesList } from "@/routes/workspace/repositories/-hooks/use-repositories-list";
import { useRepositoryDraft } from "@/routes/workspace/repositories/-hooks/use-repository-draft";
import { useUpdateRepositorySecurityMutation } from "@/routes/workspace/repositories/-hooks/use-repository-mutations";
import { cn } from "@/lib/utils";
import { VersionDenies } from "@/routes/workspace/approvals/-components/version-denies";
import { LinesInput, PolicyFlow, renderMrkdwn } from "@/routes/workspace/repositories/-components/detail/policy-flow";
import { RepositorySaveActions, SecurityPanelHeader } from "@/routes/workspace/repositories/-components/detail/settings-tab";
import { MemberList } from "@/routes/workspace/repositories/-components/member-list";

export function Security({ repo: fetched, canWrite }: { repo: Repository; canWrite: boolean }) {
  const [repo, setRepo] = useRepositoryDraft(fetched);
  const { t } = useTranslation();
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const [selectedPolicy, setSelectedPolicy] = useState<PolicySelection>("access");

  // Receivers offered as approval-notification targets (managed under
  // Notifications). The list is shared with the notifications screen, so
  // opening this tab after editing a receiver there shows the change without
  // a second fetch.
  const receivers = useReceiversList().data ?? [];
  const saveSecurityMutation = useUpdateRepositorySecurityMutation();
  // Sample-alarm preview / send state for the approval notification panel.
  const [sampleBusy, setSampleBusy] = useState<"" | "preview" | "send">("");
  const [sampleErr, setSampleErr] = useState("");
  const [preview, setPreview] = useState<NotificationSamplePreview | null>(null);
  const [sampleResults, setSampleResults] = useState<{ name: string; ok: boolean; error?: string }[] | null>(null);

  const doPreview = async () => {
    setSampleErr(""); setSampleResults(null); setSampleBusy("preview");
    try { setPreview(await api.previewRepoSample(repo.id)); }
    catch (e) { setSampleErr((e as Error).message); }
    finally { setSampleBusy(""); }
  };
  const doSendSample = async () => {
    setSampleErr(""); setPreview(null); setSampleBusy("send");
    try { setSampleResults((await api.sendRepoSample(repo.id)).results); }
    catch (e) { setSampleErr((e as Error).message); }
    finally { setSampleBusy(""); }
  };

  const save = () => {
    setError("");
    setSaved(false);
    // Send only the policy sections, on the security route: this tab is
    // reachable by a security engineer who has no rights over the upstream URL
    // or its credentials, and those must not ride along in the body.
    saveSecurityMutation.mutate(
      {
        repositoryId: repo.id,
        body: {
          // Only the six sections. The document says the server reads exactly
          // these and ignores the rest, but types config as a whole RepoConfig
          // with every field required - hence the cast. Sending the full config
          // instead would carry upstream_auth, which is the one thing this tab
          // must not touch.
          config: {
            age_policy: repo.config.age_policy,
            approval: repo.config.approval,
            vuln: repo.config.vuln,
            license: repo.config.license,
            ip_acl: repo.config.ip_acl,
            notify: repo.config.notify,
          } as Repository["config"],
        },
      },
      {
        onSuccess: () => setSaved(true),
        onError: (caught) => setError(getErrorMessage(caught)),
      },
    );
  };

  const age = repo.config.age_policy;
  // Older repos may predate the approval config section.
  const approval = repo.config.approval ?? { enabled: false, mode: "enforce", auto_approve: [] };
  const vuln = repo.config.vuln ?? { enabled: false, action: "audit", threshold: "high", ignore: [] };
  const license = repo.config.license ?? { enabled: false, action: "audit", deny: [], allow: [] };
  // Source-IP ACL applies to every repository type (including group entry).
  const ipacl = repo.config.ip_acl ?? { enabled: false, allow: [] };
  // Approval-notification receivers selected for this repository.
  const selectedReceivers = repo.config.notify?.receivers ?? [];
  const setSelectedReceivers = (next: string[]) =>
    setRepo({ ...repo, config: { ...repo.config, notify: { ...repo.config.notify, receivers: next } } });

  // Resolve each allow entry to a range/count for the breakdown shown below the
  // textarea, and a running total (capped at the safe-integer range).
  const aclRows = (ipacl.allow ?? []).map((entry) => ({ entry, info: aclEntryInfo(entry) }));
  let aclTotal = 0n;
  for (const { info } of aclRows) {
    if (info.kind === "single") aclTotal += 1n;
    else if (info.kind === "range") aclTotal += info.count;
  }
  const securityPanelClass = "mb-0 rounded-none border-0 bg-transparent py-0 shadow-none ring-0";
  const securityPanelContentClass = "px-0 py-5";
  const securityDescription = repo.type === "proxy"
    ? t("repo.security-controls-proxy-desc")
    : repo.type === "group"
      ? t("repo.security-controls-group-desc")
      : t("repo.security-controls-hosted-desc");

  return (
    <>
      {!canWrite && (
        <p className="mt-0 text-sm text-muted-foreground">
          {t("repo.readonly-note")}
        </p>
      )}

      {/* Without the admin or security action (canWrite=false) the whole form
          is disabled via fieldset[disabled], which blocks every control (mouse
          and keyboard); the reader sees the configuration but cannot change it.
          The gate is cosmetic: the security route enforces it server-side. */}
      <fieldset className="m-0 min-w-0 border-0 p-0 disabled:opacity-65" disabled={!canWrite}>

      <div className="mb-4 border-b border-border pb-3">
        <h2 className="m-0 text-xs font-semibold text-muted-foreground uppercase tracking-normal">{t("repo.security-controls")}</h2>
        <p className="mb-0 mt-1 text-xs leading-5 text-muted-foreground">{securityDescription}</p>
      </div>

      <div className="grid min-w-0 gap-6 lg:grid-cols-[minmax(300px,0.72fr)_minmax(420px,1.28fr)] lg:items-start">
      {(
        <PolicyFlow
          repo={repo}
          setRepo={setRepo}
          canWrite={canWrite}
          view="settings"
          accessOnly={repo.type === "group"}
          selectedPolicy={selectedPolicy}
          onSelectPolicy={setSelectedPolicy}
        />
      )}

      {selectedPolicy === "access" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
          <SecurityPanelHeader
            title={t("repo.source-ip-acl")}
            subtitle={t("repo.acl-subtitle")}
          />
          <fieldset className="m-0 min-w-0 border-0 p-0 transition-opacity disabled:opacity-55" disabled={!ipacl.enabled}>
          <Field className="mt-4">
            <FieldLabel>{t("repo.allowed-ips")}</FieldLabel>
            <LinesInput rows={4} placeholder={"10.0.0.0/16\n203.0.113.5\n2001:db8::/32"}
              value={ipacl.allow ?? []}
              onChange={(allow) => setRepo({ ...repo, config: { ...repo.config, ip_acl: { ...ipacl, allow } } })} />
          </Field>
          {aclRows.length > 0 && (
            <div className="mt-2.5 flex flex-col gap-1">
              {aclRows.map(({ entry, info }, i) => (
                <div key={`${entry}-${i}`} className="flex flex-wrap items-baseline gap-2 text-xs">
                  <code className="font-mono text-foreground">{entry}</code>
                  {info.kind === "invalid" && <span className="text-destructive">{t("repo.invalid-cidr")}</span>}
                  {info.kind === "single" && <span className="text-muted-foreground">{t("repo.single-host")}</span>}
                  {info.kind === "range" && <span className="text-muted-foreground">{info.first} – {info.last} · {fmtCount(info.count, info.exp)}</span>}
                </div>
              ))}
              <div className="mt-1 text-xs text-muted-foreground">
                Total allowed: {aclTotal <= ACL_MAX_SAFE
                  ? `${Number(aclTotal).toLocaleString()} ${aclTotal === 1n ? "address" : "addresses"}`
                  : "very large (includes a wide IPv6 range)"}
              </div>
            </div>
          )}
          <p className="mb-0 mt-4 text-xs leading-5 text-muted-foreground">{t("repo.acl-behavior-detail")}</p>
          </fieldset>
          </CardContent>
        </Card>
      )}

      {(repo.type === "proxy" || repo.type === "hosted") && selectedPolicy === "blocked_versions" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
            <VersionDenies repo={repo.name} showRepo={false} repoNames={[repo.name]} embedded />
          </CardContent>
        </Card>
      )}

      {repo.type === "proxy" && selectedPolicy === "age" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
          <SecurityPanelHeader
            title={t("repo.age-policy")}
          />
          <fieldset className="m-0 min-w-0 border-0 p-0 transition-opacity disabled:opacity-55" disabled={!age.enabled}>
          <FieldGroup className="grid gap-3 md:grid-cols-2">
            <Field><FieldLabel>{t("repo.min-age-placeholder")}</FieldLabel>
              <Input value={age.min_age}
                onChange={(e) => setRepo({ ...repo, config: { ...repo.config, age_policy: { ...age, min_age: e.target.value } } })} /></Field>
            <Field><FieldLabel>{t("common.action")}</FieldLabel>
              <Select value={age.action}
                onChange={(v) => setRepo({ ...repo, config: { ...repo.config, age_policy: { ...age, action: v as typeof age.action } } })}
                options={[
                  { value: "block", label: t("common.policy.block") },
                  { value: "warn", label: t("common.policy.warn") },
                ]} /></Field>
          </FieldGroup>
          </fieldset>
          </CardContent>
        </Card>
      )}

      {(repo.type === "proxy" || repo.type === "hosted") && selectedPolicy === "approval" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
          <SecurityPanelHeader
            title={t("repo.package-approval")}
            subtitle={t("repo.quarantine-subtitle")}
          />
          <fieldset className="m-0 min-w-0 border-0 p-0 transition-opacity disabled:opacity-55" disabled={!approval.enabled}>
          <FieldGroup className="grid gap-3 md:grid-cols-2">
            <Field><FieldLabel>{t("common.mode")}</FieldLabel>
              <Select value={approval.mode || "enforce"}
                onChange={(v) => setRepo({ ...repo, config: { ...repo.config, approval: { ...approval, mode: v as typeof approval.mode } } })}
                options={[
                  { value: "enforce", label: "enforce (block unapproved)" },
                  { value: "audit", label: "audit (serve, log only)" },
                ]} /></Field>
            <Field><FieldLabel>{t("repo.auto-approve-patterns")}</FieldLabel>
              <Textarea rows={3}
                value={(approval.auto_approve ?? []).join("\n")}
                placeholder={"@company/*"}
                onChange={(e) => setRepo({
                  ...repo,
                  config: {
                    ...repo.config,
                    approval: { ...approval, auto_approve: e.target.value.split("\n").map((s) => s.trim()).filter(Boolean) },
                  },
                })} /></Field>
          </FieldGroup>
          <label className="mt-4 flex items-start gap-2 text-sm">
            <Switch checked={!!approval.auto_approve_clean}
              onCheckedChange={(v) => setRepo({ ...repo, config: { ...repo.config, approval: { ...approval, auto_approve_clean: v } } })}
              aria-label={t("repo.auto-approve-clean")} />
            <span className="flex flex-col gap-0.5">
              <span>{t("repo.auto-approve-clean")}</span>
              <span className="text-xs text-muted-foreground">{t("repo.auto-approve-clean-desc")}</span>
            </span>
          </label>
          <p className="mt-4 text-xs leading-5 text-muted-foreground">Approval admits whole packages; version freshness is still gated by the age policy.</p>

          <div className="mt-5 border-t border-border pt-4">
            <h3 className="m-0 mb-2 text-sm font-semibold">
              {t("notification.title")} <span className="text-xs font-normal text-muted-foreground">{t("repo.quarantine-desc")}</span>
            </h3>
            {selectedReceivers.length > 0 && (
              <div className="flex min-w-0 items-center gap-2 max-sm:flex-wrap mb-2 flex-wrap gap-1.5">
                {selectedReceivers.map((name) => (
                  <Badge key={name} className="gap-1">
                    {name}
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-xs"
                      className="-mr-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                      title={`Remove ${name}`}
                      onClick={() => setSelectedReceivers(selectedReceivers.filter((n) => n !== name))}
                    >
                      <X className="size-3" aria-hidden="true" />
                      <span className="sr-only">Remove {name}</span>
                    </Button>
                  </Badge>
                ))}
              </div>
            )}
            {(() => {
              const options = receivers
                .filter((rcv) => !selectedReceivers.includes(rcv.name))
                .map((rcv) => ({
                  value: rcv.name,
                  label: rcv.enabled ? rcv.name : `${rcv.name} (disabled)`,
                  description: rcv.description || undefined,
                }));
              const placeholder = receivers.length === 0
                ? "No receivers - add under Notifications"
                : options.length === 0
                  ? "All receivers added"
                  : "Add notification…";
              // Disable when there is nothing to add, so the trigger keeps its
              // guidance instead of opening an empty "No options" dropdown.
              return (
                <Select value="" placeholder={placeholder} disabled={options.length === 0}
                  onChange={(v) => v && setSelectedReceivers([...selectedReceivers, v])}
                  options={options} />
              );
            })()}
            <p className="mt-2 text-sm text-muted-foreground">{t("repo.receivers-note")}</p>
            {/* Preview / sample-send only make sense once receivers exist to target. */}
            {receivers.length > 0 && (
              <div className="flex min-w-0 items-center gap-2 max-sm:flex-wrap gap-2">
                <Button variant="outline" type="button" disabled={!!sampleBusy} onClick={doPreview}>
                  {sampleBusy === "preview" ? t("common.loading") : "Preview"}
                </Button>
                <Button variant="outline" type="button" disabled={!!sampleBusy || selectedReceivers.length === 0} onClick={doSendSample}>
                  {sampleBusy === "send" ? t("common.sending") : "Send sample alarm"}
                </Button>
              </div>
            )}
            {sampleErr && <Alert className="mt-2">{sampleErr}</Alert>}
            {preview && (
              <div className="mt-2.5 rounded-[var(--radius)] border border-border bg-muted p-3">
                <div className="mb-2 text-xs text-muted-foreground">
                  Would send to: {(preview.receivers ?? []).filter((x) => x.enabled).map((x) => x.name).join(", ") || "no enabled receiver selected"}
                </div>
                {/* The rendered alarm exactly as it appears in Slack/Mattermost. */}
                <div className="rounded-[var(--radius)] border border-border bg-card p-3 text-sm leading-relaxed">
                  {renderMrkdwn(preview.payload.text)}
                </div>
                <details className="mt-2">
                  <summary className="cursor-pointer text-xs text-muted-foreground">Raw payload</summary>
                  <pre className="m-0 mt-1 overflow-x-auto text-xs">{JSON.stringify(preview.payload, null, 2)}</pre>
                </details>
              </div>
            )}
            {sampleResults && (
              <div className="mt-2.5 space-y-0.5">
                {sampleResults.map((res, i) => (
                  <div key={i} className={cn("text-xs", res.ok ? "text-muted-foreground" : "text-destructive")}>
                    {res.ok ? `✓ sent to ${res.name}` : `✗ ${res.name}: ${res.error}`}
                  </div>
                ))}
              </div>
            )}
          </div>
          </fieldset>
          </CardContent>
        </Card>
      )}

      {(repo.type === "proxy" || repo.type === "hosted") && selectedPolicy === "vulnerability" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
          <SecurityPanelHeader
            title={t("repo.vuln-policy")}
            subtitle={t("repo.vuln-subtitle")}
          />
          <fieldset className="m-0 min-w-0 border-0 p-0 transition-opacity disabled:opacity-55" disabled={!vuln.enabled}>
          <FieldGroup className="grid gap-3 md:grid-cols-2">
            <Field><FieldLabel>{t("common.threshold")}</FieldLabel>
              <Select value={vuln.threshold || "high"}
                onChange={(v) => setRepo({ ...repo, config: { ...repo.config, vuln: { ...vuln, threshold: v as typeof vuln.threshold } } })}
                options={[
                  { value: "critical", label: "critical" },
                  { value: "high", label: "high" },
                  { value: "medium", label: "medium" },
                  { value: "low", label: "low" },
                ]} /></Field>
            <Field><FieldLabel>{t("common.action")}</FieldLabel>
              <Select value={vuln.action || "audit"}
                onChange={(v) => setRepo({ ...repo, config: { ...repo.config, vuln: { ...vuln, action: v as typeof vuln.action } } })}
                options={[
                  { value: "block", label: "block (refuse to serve)" },
                  { value: "warn", label: "warn (serve, log)" },
                  { value: "audit", label: "audit (serve, log)" },
                ]} /></Field>
          </FieldGroup>
          <Field className="mt-4">
          <FieldLabel>{t("repo.ignore-advisories")}</FieldLabel>
          <Textarea rows={3}
            value={(vuln.ignore ?? []).join("\n")}
            placeholder={"CVE-2026-1234"}
            onChange={(e) => setRepo({
              ...repo,
              config: {
                ...repo.config,
                vuln: { ...vuln, ignore: e.target.value.split("\n").map((s) => s.trim()).filter(Boolean) },
              },
            })} />
          </Field>
          <label className="mt-3 flex items-center gap-2 text-sm">
            <Switch checked={!!vuln.block_unscanned}
              onCheckedChange={(v) => setRepo({ ...repo, config: { ...repo.config, vuln: { ...vuln, block_unscanned: v } } })}
              aria-label={t("repo.block-unscanned")} />
            <span>{t("repo.block-unscanned")}</span>
          </label>
          <p className="mb-0 mt-4 text-xs leading-5 text-muted-foreground">Coordinate match against OSV (direct dependency only); transitive deps and artifact integrity are out of scope. Newly disclosed advisories surface on the next re-scan.</p>
          </fieldset>
          </CardContent>
        </Card>
      )}

      {(repo.type === "proxy" || repo.type === "hosted") && selectedPolicy === "license" && (
        <Card size="sm" className={securityPanelClass}>
          <CardContent className={securityPanelContentClass}>
          <SecurityPanelHeader
            title={t("repo.license-policy")}
            subtitle={t("repo.license-subtitle")}
          />
          <fieldset className="m-0 min-w-0 border-0 p-0 transition-opacity disabled:opacity-55" disabled={!license.enabled}>
          <FieldGroup className="grid gap-3 md:grid-cols-2">
            <Field><FieldLabel>{t("common.action")}</FieldLabel>
              <Select value={license.action || "audit"}
                onChange={(v) => setRepo({ ...repo, config: { ...repo.config, license: { ...license, action: v as typeof license.action } } })}
                options={[
                  { value: "block", label: "block (refuse to serve)" },
                  { value: "warn", label: "warn (serve, log)" },
                  { value: "audit", label: "audit (serve, log)" },
                ]} /></Field>
            <Field><FieldLabel>{t("repo.deny-licenses")}</FieldLabel>
              <Textarea rows={3}
                value={(license.deny ?? []).join("\n")}
                placeholder={"GPL-3.0\nAGPL-3.0"}
                onChange={(e) => setRepo({
                  ...repo,
                  config: {
                    ...repo.config,
                    license: { ...license, deny: e.target.value.split("\n").map((s) => s.trim()).filter(Boolean) },
                  },
                })} /></Field>
            <Field><FieldLabel>{t("repo.allow-licenses")}</FieldLabel>
              <Textarea rows={3}
                value={(license.allow ?? []).join("\n")}
                placeholder={"MIT\nApache-2.0\nBSD-3-Clause"}
                onChange={(e) => setRepo({
                  ...repo,
                  config: {
                    ...repo.config,
                    license: { ...license, allow: e.target.value.split("\n").map((s) => s.trim()).filter(Boolean) },
                  },
                })} /></Field>
          </FieldGroup>
          <label className="mt-3 flex items-center gap-2 text-sm">
            <Switch checked={!!license.block_unresolved}
              onCheckedChange={(v) => setRepo({ ...repo, config: { ...repo.config, license: { ...license, block_unresolved: v } } })}
              aria-label={t("repo.block-unresolved")} />
            <span>{t("repo.block-unresolved")}</span>
          </label>
          <p className="mb-0 mt-4 text-xs leading-5 text-muted-foreground">{t("repo.license-note")}</p>
          </fieldset>
          </CardContent>
        </Card>
      )}
      </div>

      {repo.type === "hosted" && repo.format === "pypi" && (
        <Card size="sm" className="mt-6">
          <CardContent>
            <h3 className="m-0 text-sm font-semibold">{t("repo.pypi-compatibility")}</h3>
            <label className="mt-4 flex items-start gap-2 text-sm">
              <Switch checked={Boolean(repo.config.upload?.pypi_allow_legacy_zip)}
                onCheckedChange={(value) => setRepo({ ...repo, config: { ...repo.config, upload: { ...repo.config.upload, pypi_allow_legacy_zip: value } } })}
                aria-label={t("repo.pypi-legacy-zip")} />
              <span><span className="block font-medium">{t("repo.pypi-legacy-zip")}</span><span className="mt-1 block text-xs text-muted-foreground">{t("repo.pypi-legacy-zip-help")}</span></span>
            </label>
          </CardContent>
        </Card>
      )}

      {(
        <div className="mt-8">
          <PolicyFlow
            repo={repo}
            setRepo={setRepo}
            canWrite={canWrite}
            view="flow"
            accessOnly={repo.type === "group"}
            selectedPolicy={selectedPolicy}
            onSelectPolicy={setSelectedPolicy}
          />
        </div>
      )}

      </fieldset>

      {error && <Alert className="mb-4">{error}</Alert>}
      {canWrite && <RepositorySaveActions saved={saved} onSave={save} />}
    </>
  );
}

// GroupMembers edits a group repository's ordered member list. Changes apply
// on Save like the other settings panels.
export function GroupMembers({
  repo,
  setRepo,
}: {
  repo: Repository;
  // The caller's draft setter: editing members is an unsaved change until the
  // settings form is saved, so this does not write through to the server.
  setRepo: RepositoryDraftSetter;
}) {
  const { t } = useTranslation();
  const { repositories: repos } = useRepositoriesList();

  const members = repo.config.group?.members ?? [];
  const setMembers = (m: string[]) =>
    setRepo({ ...repo, config: { ...repo.config, group: { members: m } } });
  const candidates = repos.filter(
    (r) => r.format === repo.format && r.type !== "group" && r.name !== repo.name && !members.includes(r.name),
  );

  return (
    <Card size="sm" className="mb-4">
      <CardContent>
      <h2 className="m-0 mb-4 text-base font-semibold">{t("repo.members")} <span className="text-xs font-normal text-muted-foreground">{t("repo.members-subtitle")}</span></h2>
      <MemberList members={members} onChange={setMembers}
        repoIndex={Object.fromEntries(repos.map((r) => [r.name, r.id]))}
        repoTypes={Object.fromEntries(repos.map((r) => [r.name, r.type]))} />
      <div className="flex min-w-0 items-center gap-2 mt-3 max-sm:flex-wrap items-stretch max-sm:flex-col">
        <Select value="" placeholder={t("repo.add-member-placeholder")}
          onChange={(v) => v && setMembers([...members, v])}
          options={candidates.map((r) => ({ value: r.name, label: `${r.name} (${r.type})` }))} />
      </div>
      </CardContent>
    </Card>
  );
}

// RepoPermissions lists, at a glance, which roles grant access to this
// repository (every role permission whose pattern matches the repo name),
// read-only. Admin-only tab. Assignment itself is managed on the Roles pages.
