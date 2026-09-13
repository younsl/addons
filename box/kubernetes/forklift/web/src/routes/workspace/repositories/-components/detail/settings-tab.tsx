import { useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "@/api";
import type { Repository } from "@/services/v1/openapi-types";
import { Alert } from "@/components/app-ui/alert";
import { LockNote } from "@/components/app-ui/lock-note";
import { StateBadge } from "@/components/app-ui/status-badge";
import { UpstreamAuthFields } from "@/components/app-ui/upstream-auth-fields";
import { UpstreamStatus } from "@/components/feedback/upstream-status";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import { useTranslation } from "@/lib/i18n";
import { useRepositoryDraft } from "@/routes/workspace/repositories/-hooks/use-repository-draft";
import {
  useDeleteRepositoryMutation,
  useSetRepositoryDisabledMutation,
  useUpdateRepositoryMutation,
} from "@/routes/workspace/repositories/-hooks/use-repository-mutations";
import { GroupMembers } from "@/routes/workspace/repositories/-components/detail/security-tab";

export function RepositorySaveActions({ saved, onSave }: { saved: boolean; onSave: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="mt-4 flex min-w-0 items-center gap-3">
      <Button onClick={onSave}>{t("common.save-changes")}</Button>
      {saved && <span className="text-sm text-muted-foreground">{t("common.saved")}</span>}
    </div>
  );
}

export function Settings({ repo: fetched, canWrite }: { repo: Repository; canWrite: boolean }) {
  // The form edits a draft; the query keeps the server's copy. See
  // useRepositoryDraft for why the two must not be the same object.
  const [repo, setRepo] = useRepositoryDraft(fetched);
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmPurge, setConfirmPurge] = useState(false);
  const [purged, setPurged] = useState<number | null>(null);
  const updateMutation = useUpdateRepositoryMutation();
  const deleteMutation = useDeleteRepositoryMutation();
  const setDisabledMutation = useSetRepositoryDisabledMutation();
  const queryClient = useQueryClient();

  const purge = async () => {
    setError("");
    setPurged(null);
    try {
      const { deleted } = await api.purgeArtifacts(repo.id);
      setConfirmPurge(false);
      setPurged(deleted);
      // Emptying a repository changes its artifact count, its size and the
      // Artifacts tab. None of that was refreshed before, so the numbers stayed
      // at their pre-purge values until a reload.
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositories() }),
        queryClient.invalidateQueries({
          queryKey: operationKeyPrefix(
            openApiQueryKeys.listRepositoryArtifacts({ path: { id: repo.id } }),
          ),
        }),
      ]);
    } catch (e) {
      setError(getErrorMessage(e));
      setConfirmPurge(false);
    }
  };

  const save = () => {
    setError("");
    setSaved(false);
    updateMutation.mutate(
      { repositoryId: repo.id, body: { upstream_url: repo.upstream_url, config: repo.config } },
      {
        onSuccess: () => setSaved(true),
        onError: (caught) => setError(getErrorMessage(caught)),
      },
    );
  };

  const del = () => {
    deleteMutation.mutate(repo.id, {
      onSuccess: () => navigate({ to: "/workspace/repositories" }),
      // Deleting can be refused - a seeded repository returns 403 - so the
      // failure has to stay on the page rather than navigating away silently.
      onError: (caught) => { setError(getErrorMessage(caught)); setConfirmDelete(false); },
    });
  };

  const cache = repo.config.cache;

  return (
    <>
      {!canWrite && (
        <p className="mt-0 text-sm text-muted-foreground">
          {t("repo.readonly-note")}
        </p>
      )}
      <ConfirmModal
        open={confirmDelete}
        title={`Delete repository "${repo.name}"?`}
        message={t("repo.delete-confirm")}
        confirmLabel={t("common.delete")}
        danger
        onConfirm={del}
        onCancel={() => setConfirmDelete(false)}
      />

      {/* For an auditor (canWrite=false) the whole settings form is disabled via
          fieldset[disabled], which blocks every control (mouse and keyboard);
          they can read the configuration but not change it. */}
      <fieldset className="m-0 min-w-0 border-0 p-0 disabled:opacity-65" disabled={!canWrite}>
      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("common.state")}</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {repo.disabled
            ? t("repo.offline-note")
            : t("repo.online-note")}
        </p>
        {canWrite ? (
          <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
            <Switch
              checked={!repo.disabled}
              onCheckedChange={(online) => {
                setError("");
                // Taking a repository offline is a server-state change, not a
                // draft edit: the mutation invalidates the query and the new
                // value arrives back through it.
                setDisabledMutation.mutate(
                  { repositoryId: repo.id, disabled: !online },
                  { onError: (caught) => setError(getErrorMessage(caught)) },
                );
              }}
              aria-label={repo.disabled ? t("repo.offline") : t("repo.online")}
            />
            <span>{repo.disabled ? t("repo.offline") : t("repo.online")}</span>
          </label>
        ) : (
          <StateBadge state={repo.disabled ? "offline" : "online"}>{repo.disabled ? t("repo.offline") : t("repo.online")}</StateBadge>
        )}
        </CardContent>
      </Card>

      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("repo.visibility")}</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">{t("repo.public-access-note")}</p>
        <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
          <Switch
            checked={Boolean(repo.config.public)}
            disabled={!canWrite}
            onCheckedChange={(isPublic) => {
              setError("");
              // Visibility saves on the switch rather than waiting for Save:
              // it is one boolean, and leaving a repository silently public
              // until an unrelated Save is pressed is the wrong failure mode.
              const next = { ...repo, config: { ...repo.config, public: isPublic } };
              setRepo(next);
              updateMutation.mutate(
                { repositoryId: repo.id, body: { upstream_url: next.upstream_url, config: next.config } },
                {
                  onError: (caught) => { setError(getErrorMessage(caught)); setRepo(repo); },
                },
              );
            }}
            aria-label={repo.config.public ? t("repo.public") : t("repo.private")}
          />
          <span>{repo.config.public ? t("repo.public") : `${t("repo.private")} (${t("common.default")})`}</span>
        </label>
        </CardContent>
      </Card>

      {(repo.type === "proxy" || repo.type === "group") && (
        <h2 className="mt-6 mb-3 border-b border-border pb-[7px] text-xs font-semibold text-muted-foreground uppercase tracking-normal">{t("common.repository")}</h2>
      )}

      {repo.type === "proxy" && (
        <Card size="sm" className="mb-4">
          <CardContent>
          <div className="mb-4 flex items-start justify-between gap-3 max-sm:flex-col max-sm:items-stretch">
            <h2 className="m-0 text-base font-semibold">{t("common.upstream")}</h2>
            <UpstreamStatus repoId={repo.id} withButton />
          </div>
          <Field>
            <FieldLabel>{t("repo.upstream-url")}</FieldLabel>
            <Input value={repo.upstream_url}
              onChange={(e) => setRepo({ ...repo, upstream_url: e.target.value })} />
          </Field>
          <div className="mt-5 border-t border-border pt-4">
            <UpstreamAuthFields
              value={repo.config.upstream_auth ?? {}}
              onChange={(upstream_auth) => setRepo({ ...repo, config: { ...repo.config, upstream_auth } })}
            />
          </div>
          </CardContent>
        </Card>
      )}

      {repo.type === "group" && <GroupMembers repo={repo} setRepo={setRepo} />}

      {repo.type === "proxy" && (
        <Card size="sm" className="mb-4">
          <CardContent>
          <h2 className="m-0 mb-4 text-base font-semibold">{t("common.cache")}</h2>
          <label className="mb-4 flex items-center gap-2 text-sm">
            <Checkbox checked={cache.enabled}
              onCheckedChange={(checked) => setRepo({ ...repo, config: { ...repo.config, cache: { ...cache, enabled: !!checked } } })} />
            <span>{t("repo.enable-caching")}</span>
          </label>
          <FieldGroup className="grid gap-4 md:grid-cols-3">
            <Field><FieldLabel>{t("repo.metadata-ttl")}</FieldLabel>
              <Input value={cache.metadata_ttl}
                onChange={(e) => setRepo({ ...repo, config: { ...repo.config, cache: { ...cache, metadata_ttl: e.target.value } } })} /></Field>
            <Field><FieldLabel>{t("repo.negative-ttl")}</FieldLabel>
              <Input value={cache.negative_ttl}
                onChange={(e) => setRepo({ ...repo, config: { ...repo.config, cache: { ...cache, negative_ttl: e.target.value } } })} /></Field>
            <Field><FieldLabel>{t("repo.max-size")}</FieldLabel>
              <Input type="number" value={cache.max_size_bytes}
                onChange={(e) => setRepo({ ...repo, config: { ...repo.config, cache: { ...cache, max_size_bytes: Number(e.target.value) } } })} /></Field>
          </FieldGroup>
          </CardContent>
        </Card>
      )}

      {repo.type !== "group" && (
        <Card size="sm" className="mb-4">
          <CardContent>
          <h2 className="m-0 mb-4 text-base font-semibold">{t("repo.artifact-retention")} <span className="text-xs font-normal text-muted-foreground">{t("repo.retention-subtitle")}</span></h2>
          <Field>
            <FieldLabel>{t("repo.idle-ttl")}</FieldLabel>
              <Input value={repo.config.retention?.idle_ttl ?? ""}
                placeholder="0"
                onChange={(e) => setRepo({ ...repo, config: { ...repo.config, retention: { idle_ttl: e.target.value } } })} />
          </Field>
          <p className="mb-0 mt-4 text-sm text-muted-foreground">Artifacts not served for this long are deleted automatically; each removal is recorded in the audit log. Based on last-served time.</p>
          </CardContent>
        </Card>
      )}

      </fieldset>

      {error && <Alert className="mb-4">{error}</Alert>}
      {canWrite && <RepositorySaveActions saved={saved} onSave={save} />}

      {canWrite && (
      <>
      <ConfirmModal
        open={confirmPurge}
        title={`Purge all artifacts in "${repo.name}"?`}
        message={t("repo.purge-confirm")}
        confirmLabel={t("repo.purge")}
        danger
        onConfirm={purge}
        onCancel={() => setConfirmPurge(false)}
      />

      <Card size="sm" className="mb-4 mt-4 ring-destructive">
        <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold text-destructive">{t("common.danger-zone")}</h2>
        {repo.type !== "group" && (
          <>
            <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">{t("repo.purge-note")}</p>
            {purged !== null && <div className="mb-3 text-sm text-muted-foreground">Purged {purged} {purged === 1 ? "artifact" : "artifacts"}.</div>}
            <Button variant="destructive" onClick={() => setConfirmPurge(true)}>{t("repo.purge-all")}</Button>
            <div className="my-4 border-t border-border" />
          </>
        )}
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">{t("repo.delete-note")}</p>
        <Button variant="destructive" disabled={repo.seeded} onClick={() => setConfirmDelete(true)}>{t("repo.delete")}</Button>
        {repo.seeded && (
          <LockNote title={t("repo.delete-locked-title")}>{t("repo.delete-seeded-note")}</LockNote>
        )}
        </CardContent>
      </Card>
      </>
      )}
    </>
  );
}

export function SecurityPanelHeader({
  title,
  subtitle,
  toggleLabel,
  checked,
  onCheckedChange,
}: {
  title: string;
  subtitle?: string;
  toggleLabel?: string;
  checked?: boolean;
  onCheckedChange?: (checked: boolean) => void;
}) {
  const { t } = useTranslation();
  const statusLabel = checked ? t("repo.setting-enabled") : t("repo.setting-disabled");

  return (
    <div className="mb-5 min-w-0">
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
        <h2 className="m-0 text-sm font-semibold text-foreground">{title}</h2>
        {subtitle && <span className="text-xs text-muted-foreground">{subtitle}</span>}
      </div>
      {toggleLabel && onCheckedChange && (
        <div className="mt-2 flex min-w-0 items-center gap-2">
          <Switch checked={!!checked} onCheckedChange={onCheckedChange} aria-label={`${toggleLabel}: ${statusLabel}`} />
          <span className="text-xs font-medium text-muted-foreground">{statusLabel}</span>
          <span className="min-w-0 text-xs leading-5 text-muted-foreground">{toggleLabel}</span>
        </div>
      )}
    </div>
  );
}

// Security collects everything that governs who may reach the repository and
// what is allowed to pass: the source-IP allow list (every repo type) and, for
// a proxy, the supply-chain gates (age, package approval, vulnerability and
// license policy). Saving writes the whole repo config, same as Settings.
