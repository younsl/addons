import { useState, type ReactNode } from "react";
import { createFileRoute, Link } from "@tanstack/react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ExternalLink, VolumeX } from "lucide-react";
import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { GitlabCiView } from "@/components/app-ui/gitlab-ci-view";
import { WiringMark } from "./-dashboard";
import { CopyOnHover } from "@/components/app-ui/copy-button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import {
  Table,
  TableBody,
  TableCell,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { openApiQueryKeys } from "@/query/v1/openapi-query-keys";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { updateCoverageProjectMute } from "@/services/v1/coverage/api";
import { useLanguage, useTranslation, type MessageKey } from "@/lib/i18n";
import {
  isMuted,
  MUTE_SCOPES,
  muteScopeHintKey,
  muteScopeLabelKey,
  readExclusionReason,
  toggleMuteScope,
} from "@/lib/coverage-exclusion";
import { normalizeHost } from "@/lib/coverage-host";
import { formatAbsolute, formatRelative } from "@/utils/format-relative";

export const Route = createFileRoute("/workspace/coverage/project")({
  // A GitLab path contains slashes, so it travels as a search param rather than
  // a route segment.
  validateSearch: (search: Record<string, unknown>): { path: string } => ({
    path: typeof search.path === "string" ? search.path : "",
  }),
  component: CoverageProjectRoute,
});

const appliedVariant: Record<string, "success" | "warning" | "destructive" | "default"> = {
  yes: "success",
  partial: "warning",
  no: "destructive",
  error: "destructive",
};

const appliedLabelKey: Record<string, MessageKey> = {
  yes: "coverage.applied-yes",
  partial: "coverage.applied-partial",
  no: "coverage.applied-no",
  error: "coverage.applied-error",
};

function CoverageProjectRoute() {
  const { t } = useTranslation();
  const { me } = useAuth();
  const { path } = Route.useSearch();
  const queryClient = useQueryClient();
  const language = useLanguage();
  const [showPipeline, setShowPipeline] = useState(false);

  const params = { query: { path } };
  const { data: project, error, isLoading } = useQuery({
    ...openApiQueryOptions.getCoverageProject(params),
    enabled: !!path,
  });
  const { data: overview } = useQuery(openApiQueryOptions.getCoverage());
  const { data: lastCommit } = useQuery({
    ...openApiQueryOptions.getCoverageProjectLastCommit(params),
    enabled: !!path,
    // A project with no branch has no commit; that is a normal answer, not a
    // failure worth retrying.
    retry: false,
  });
  const { data: pipeline, isFetching: pipelineLoading } = useQuery({
    ...openApiQueryOptions.getCoverageProjectPipeline(params),
    enabled: showPipeline && me.admin && !!path,
  });

  // The whole selection travels on every change, so what is left out is what
  // comes back into the measurement.
  const exclusion = useMutation({
    mutationFn: (scopes: string[]) =>
      updateCoverageProjectMute({ query: { path }, body: { scopes: scopes as ("ci" | "registry")[] } }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverageProject(params) });
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getCoverage() });
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listCoverageGroups() });
    },
  });

  if (!path) {
    return (
      <>
        <PageHeader title={t("coverage.project")} />
        <Alert>{t("coverage.project-path-missing")}</Alert>
      </>
    );
  }
  if (isLoading) return <div className="p-4 text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (error || !project) {
    return (
      <>
        <PageHeader title={path} />
        <Alert>{t("coverage.project-not-found")}</Alert>
      </>
    );
  }

  const excluded = !!project.exclude_reason;
  const mutedScopes = project.muted_scopes ?? [];
  // Muted halves are worth saying out loud only while the project is still
  // being measured; once it is out entirely, the banner below already says so.
  const partiallyMuted = !excluded && mutedScopes.length > 0;

  const reason = readExclusionReason(project.exclude_reason);
  const excludeReasonText = reason ? `${t(reason.key)}${reason.topic ? ` ${reason.topic}` : ""}` : "";

  return (
    <>
      <p className="mb-3">
        <Link
          to="/workspace/coverage"
          className="inline-flex items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground hover:no-underline"
        >
          <ArrowLeft className="size-4" aria-hidden="true" />
          {t("coverage.back-to-list")}
        </Link>
      </p>
      <PageHeader
        title={<span className="block min-w-0 truncate">{project.name}</span>}
        actions={
          <>
            {project.web_url && (
              <a
                href={project.web_url}
                target="_blank"
                rel="noreferrer"
                className={buttonVariants({ variant: "outline" })}
              >
                <ExternalLink className="size-4" aria-hidden="true" />
                GitLab
              </a>
            )}
            {me.admin && (
              <Popover>
                <PopoverTrigger
                  render={<Button type="button" variant={mutedScopes.length > 0 ? "default" : "outline"} />}
                >
                  <VolumeX className="size-4" aria-hidden="true" />
                  {t("coverage.mute")}
                  {mutedScopes.length > 0 && (
                    <span className="tabular-nums opacity-80">
                      {mutedScopes.length}/{MUTE_SCOPES.length}
                    </span>
                  )}
                </PopoverTrigger>
                <PopoverContent align="end">
                  <PopoverHeader>
                    <PopoverTitle>{t("coverage.mute-scopes")}</PopoverTitle>
                    <PopoverDescription>{t("coverage.mute-scopes-hint")}</PopoverDescription>
                  </PopoverHeader>
                  {/* Each check carries what it reads, spelled out rather than
                      hidden behind a tooltip: this is where somebody decides to
                      stop requiring it, so the basis for that decision belongs
                      on screen. */}
                  {MUTE_SCOPES.map((scope) => (
                    <label key={scope} className="flex items-start gap-2 text-sm">
                      <Checkbox
                        className="mt-0.5"
                        checked={isMuted(mutedScopes, scope)}
                        disabled={exclusion.isPending}
                        onCheckedChange={(checked) =>
                          exclusion.mutate(toggleMuteScope(mutedScopes, scope, Boolean(checked)))
                        }
                      />
                      <span className="min-w-0">
                        <span className="block">{t(muteScopeLabelKey[scope])}</span>
                        <span className="block text-xs text-muted-foreground">
                          {t(muteScopeHintKey[scope])}
                        </span>
                      </span>
                    </label>
                  ))}
                  {/* Clearing every check at once, so coming back from a full
                      exclusion is one action rather than one per check. */}
                  {mutedScopes.length > 0 && (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={exclusion.isPending}
                      onClick={() => exclusion.mutate([])}
                    >
                      {t("coverage.unmute")}
                    </Button>
                  )}
                </PopoverContent>
              </Popover>
            )}
          </>
        }
      />
      <PageDescription className="mb-4 font-mono text-[13px]">{project.path}</PageDescription>

      {exclusion.isError && <Alert className="mb-4">{(exclusion.error as Error).message}</Alert>}
      {/* An excluded project still shows its verdict, because the verdict is
          what makes the opt-out reversible. That makes it worth saying plainly
          that none of it is being counted, and on whose authority. */}
      {excluded && (
        <div className="mb-4 flex items-start gap-2 rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel-raised)] px-3 py-2.5 text-sm">
          <VolumeX className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          {/* Title, consequence and reason on their own lines: concatenating
              them into one sentence does not survive translation. */}
          <span className="min-w-0">
            <span className="block font-medium">{t("coverage.muted-title")}</span>
            <span className="block">{t("coverage.muted-banner")}</span>
            <span className="block text-muted-foreground">{excludeReasonText}</span>
          </span>
        </div>
      )}
      {partiallyMuted && (
        <div className="mb-4 flex items-start gap-2 rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel-raised)] px-3 py-2.5 text-sm">
          <VolumeX className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          <span className="min-w-0">
            <span className="block">{t("coverage.muted-scopes-banner")}</span>
            <span className="block text-muted-foreground">
              {mutedScopes.map((scope) => t(muteScopeLabelKey[scope])).join(", ")}
            </span>
          </span>
        </div>
      )}
      {!project.scanned && (
        <div className="mb-4 rounded-md border border-accent-ink/40 bg-primary/10 px-3 py-2 text-sm">
          {t("coverage.not-in-last-scan")}
        </div>
      )}

      <Card size="sm">
        <CardContent>
          <TableWrap>
            <Table className="min-w-0">
              <TableBody>
                <Row label={t("coverage.project")}>
                  <CopyOnHover value={project.path}>
                    <span className="font-mono text-[13px]">{project.path}</span>
                  </CopyOnHover>
                </Row>
                <Row label={t("coverage.status")}>
                  {excluded ? (
                    <span className="flex flex-wrap items-center gap-2">
                      <span className="inline-flex items-center gap-1.5 text-muted-foreground">
                        <VolumeX className="size-3.5 shrink-0" aria-hidden="true" />
                        {t("coverage.muted-title")}
                      </span>
                      <span className="text-muted-foreground">{excludeReasonText}</span>
                    </span>
                  ) : project.skipped ? (
                    <span className="text-muted-foreground">{t("coverage.no-ci")}</span>
                  ) : (
                    <Badge variant={appliedVariant[project.applied] ?? "default"}>
                      {t(appliedLabelKey[project.applied] ?? "coverage.applied-no")}
                    </Badge>
                  )}
                </Row>
                <Row label={t("coverage.wiring")}>
                  {/* The same mark the list uses, so one project does not read as
                      two different states depending on which page is open, with
                      what each half actually reads spelled out underneath. This
                      page has the room the column did not. */}
                  <span className="flex flex-col gap-3">
                    {MUTE_SCOPES.map((scope) => (
                      <span key={scope} className="min-w-0">
                        <WiringMark
                          on={scope === "ci" ? project.ci_wired : project.registry_pinned}
                          label={t(muteScopeLabelKey[scope])}
                          muted={isMuted(mutedScopes, scope)}
                        />
                        <span className="mt-0.5 block text-xs text-muted-foreground">
                          {t(muteScopeHintKey[scope])}
                        </span>
                      </span>
                    ))}
                  </span>
                </Row>
                <Row label={t("coverage.format")}>{project.format || "-"}</Row>
                <Row label={t("coverage.branch")}>
                  {project.branch || project.default_branch || "-"}
                  {project.on_default === false && (
                    <Badge variant="outline" className="ml-1.5">{t("coverage.not-default")}</Badge>
                  )}
                </Row>
                <Row label={t("coverage.evidence")}>
                  {project.evidence.length === 0 ? (
                    <span className="text-muted-foreground">{t("coverage.no-evidence")}</span>
                  ) : (
                    <ul className="m-0 list-none space-y-0.5 p-0">
                      {project.evidence.map((file) => (
                        <li key={file} className="font-mono text-[12px] text-muted-foreground">{file}</li>
                      ))}
                    </ul>
                  )}
                </Row>
                {project.topics.length > 0 && (
                  <Row label={t("coverage.topics")}>
                    <span className="flex flex-wrap gap-1">
                      {project.topics.map((topic) => (
                        <Badge key={topic} variant="outline">{topic}</Badge>
                      ))}
                    </span>
                  </Row>
                )}
                {project.note && <Row label={t("coverage.note")}>{project.note}</Row>}
                <Row label={t("coverage.last-activity")}>
                  {/* The full instant with its zone. The table's column answers
                      "is this stale" with a relative age, which is what a column
                      has room for; this page has room for the exact answer, and
                      an exact time without a zone is ambiguous to whoever reads
                      it from somewhere else. */}
                  {project.last_activity_at ? (
                    <span className="flex flex-wrap items-baseline gap-2">
                      <span>{formatAbsolute(project.last_activity_at, language)}</span>
                      <span className="text-xs text-muted-foreground">
                        {formatRelative(project.last_activity_at)}
                      </span>
                    </span>
                  ) : (
                    "-"
                  )}
                </Row>
                {lastCommit && (
                  <Row label={t("coverage.last-commit")}>
                    {/* Subject on the left, author on the right. The commit time
                        is left out: the row above already carries the project's
                        last activity, and two timestamps a line apart invite a
                        comparison that means nothing. */}
                    <span className="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-1">
                      <span className="flex min-w-0 items-baseline gap-2">
                        <code className="shrink-0 font-mono text-[12px]">{lastCommit.short_id}</code>
                        <span className="min-w-0">{lastCommit.title}</span>
                      </span>
                      <span className="shrink-0 text-xs text-muted-foreground">
                        {lastCommit.author_name}
                      </span>
                    </span>
                  </Row>
                )}
                {/* The two addresses the verdict was reached between: the GitLab
                    instance the configuration was read from, and the forklift
                    host it had to name to count. Reading a verdict without them
                    means trusting that both were the ones you had in mind. */}
                {overview?.gitlab_url && (
                  <Row label={t("coverage.gitlab-host")}>
                    {/* Shown as a bare host, the way the forklift host below it
                        is: the row names an instance, and the scheme is noise
                        next to it. The link and the copy still carry the full
                        URL, which is what either one is for. */}
                    <CopyOnHover value={overview.gitlab_url}>
                      <a
                        href={overview.gitlab_url}
                        target="_blank"
                        rel="noreferrer"
                        className="font-mono text-[13px] hover:underline"
                      >
                        {normalizeHost(overview.gitlab_url)}
                      </a>
                    </CopyOnHover>
                  </Row>
                )}
                {overview?.forklift_host && (
                  <Row label={t("coverage.forklift-host")}>
                    <CopyOnHover value={overview.forklift_host}>
                      <span className="font-mono text-[13px]">{overview.forklift_host}</span>
                    </CopyOnHover>
                  </Row>
                )}
              </TableBody>
            </Table>
          </TableWrap>
        </CardContent>
      </Card>

      {me.admin && (
        <section className="mt-6">
          <div className="mb-2 flex items-center justify-between gap-3">
            <h2 className="text-sm font-medium">{t("coverage.pipeline")}</h2>
            <Button variant="outline" size="sm" onClick={() => setShowPipeline((v) => !v)}>
              {showPipeline ? t("coverage.hide-pipeline") : t("coverage.show-pipeline")}
            </Button>
          </div>
          {showPipeline && pipelineLoading && (
            <p className="text-sm text-muted-foreground">{t("common.loading")}</p>
          )}
          {showPipeline && pipeline && pipeline.files.length === 0 && (
            <p className="text-sm text-muted-foreground">{t("coverage.no-pipeline-files")}</p>
          )}
          {showPipeline &&
            pipeline?.files.map((file) => (
              <div key={file.path} className="mb-3">
                <div className="mb-1 flex flex-wrap items-center gap-2 text-xs">
                  {/* The filename opens the file in GitLab, on the ref the
                      viewer is showing. Reading a pipeline here usually ends in
                      editing it there, and retyping the path into GitLab is the
                      step in between. */}
                  {project.web_url && pipeline?.ref ? (
                    <a
                      href={blobURL(project.web_url, pipeline.ref, file.path)}
                      target="_blank"
                      rel="noreferrer"
                      className="inline-flex items-center gap-1 hover:underline"
                    >
                      <code className="font-mono">{file.path}</code>
                      <ExternalLink className="size-3 shrink-0 opacity-60" aria-hidden="true" />
                    </a>
                  ) : (
                    <code className="font-mono">{file.path}</code>
                  )}
                  {file.matches_forklift && <Badge variant="success">{t("coverage.references-forklift")}</Badge>}
                  {file.truncated && <Badge variant="outline">{t("coverage.truncated")}</Badge>}
                </div>
                <GitlabCiView
                  path={file.path}
                  content={file.content}
                  forkliftHost={overview?.forklift_host ?? ""}
                />
              </div>
            ))}
        </section>
      )}

    </>
  );
}

// blobURL points at one file on one ref in GitLab. Each path segment is encoded
// on its own so a branch like "release/1.2" keeps its slashes, which are what
// makes the URL address the right ref rather than a nested one.
function blobURL(webURL: string, ref: string, path: string): string {
  const encode = (value: string) => value.split("/").map(encodeURIComponent).join("/");
  return `${webURL.replace(/\/+$/, "")}/-/blob/${encode(ref)}/${encode(path)}`;
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <TableRow>
      <TableCell className="w-44 align-top text-muted-foreground">{label}</TableCell>
      <TableCell className="align-top">{children}</TableCell>
    </TableRow>
  );
}
