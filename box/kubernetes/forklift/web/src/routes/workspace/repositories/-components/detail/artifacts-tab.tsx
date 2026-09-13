import { useQuery, useQueryClient } from "@tanstack/react-query";
// The generated Artifact, not the hand-written one. They differ on
// blob_missing_statuses: the document $refs StatusCount (code and count
// required) for DanglingRef.statuses but inlines a weaker copy here, with
// neither required. The generated type is faithful to the document; the
// document is inconsistent with itself, which is an openapi-side fix.
import type { Artifact } from "@/services/v1/openapi-types";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { operationKeyPrefix } from "@/query/query-key-prefix";
import { auditTime } from "@/routes/workspace/repositories/-components/detail/audit-logs-tab";
import { Link, useNavigate } from "@tanstack/react-router";
import { ChevronDown, Lock, TriangleAlert, Upload, X } from "lucide-react";
import { ReactNode, useEffect, useState } from "react";
import { ArtifactPublication, api, deleteArtifactPublication, setCargoPublicationYanked } from "@/api";
import {
  postBulkDeleteRepositoryArtifacts,
  postBulkRepositoryArtifactsLabels,
} from "@/services/v1/repositories/api";
import { bulkRetryPaths, runArtifactBulk, type ArtifactBulkOutcome } from "@/lib/artifact-bulk";
import type { RepositoryListItem } from "@/services/v1/openapi-types";
import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { ArtifactLabels } from "@/components/app-ui/artifact-labels";
import { Badge } from "@/components/app-ui/badge";
import { CopyOnHover } from "@/components/app-ui/copy-button";
import { SeverityBar } from "@/components/app-ui/severity-bar";
import { SortableHead, Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableWrap, useSort } from "@/components/app-ui/table";
import { TablePager, TableSearchControls, highlightMatches, useTableSearch } from "@/components/app-ui/table-search";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { MenuItem } from "@/components/ui/menu-item";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { MessageKey, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatFileSize } from "@/utils/format-file-size";

const ART_SEV_RANK: Record<string, number> = { critical: 5, high: 4, medium: 3, low: 2, none: 1 };

const ARTIFACTS_PAGE_SIZE = 50;

// artifactTime renders a stored RFC3339 timestamp the way the artifacts table
// shows it, matching the server-side search's rendering of the same column.
export function artifactTime(iso?: string): string {
  return iso?.slice(0, 19).replace("T", " ") ?? "";
}

// BrokenArtifact warns that an artifact's bytes are gone from storage, so any
// request for it fails. The remedy differs by repository type, which is the part
// a user actually needs: a hosted artifact has to be published again, while a
// proxy's cached copy can simply be deleted and re-fetched from the upstream.
// failedOpLabel names the operation a response code came from. A reader of this
// warning is not debugging HTTP: they want to know whether it was an install or a
// publish that broke.
function failedOpLabel(code: number, t: (key: MessageKey) => string): string {
  return code === 500 ? t("repo.failed-op-download")
    : code === 503 ? t("repo.failed-op-publish")
      : t("repo.failed-op-other");
}

// useUserIds maps username -> id so a name can link to its user detail page.
// Listing users needs the auditor privilege, so a reader without it simply gets
// plain text instead of a link they could not follow anyway.
export function useUserIds(enabled: boolean): Record<string, number> {
  const usersQuery = useQuery({
    ...openApiQueryOptions.listUsers(),
    enabled,
    // A reader without the privilege gets an empty map and plain text, which
    // is the right outcome, not an error worth reporting.
    meta: { suppressGlobalErrorToast: true },
  });

  return Object.fromEntries((usersQuery.data ?? []).map((user) => [user.username, user.id]));
}

// userLink renders a username as a link to its detail page when the id is known,
// falling back to plain text. label overrides the rendered text, e.g. to carry
// search-match highlighting.
export function userLink(username: string, ids: Record<string, number>, label?: ReactNode): ReactNode {
  const id = ids[username];
  return id ? <Link to="/access/users/$id" params={{ id: String(id) }}>{label ?? username}</Link> : (label ?? username);
}

// BrokenPublication marks a published version whose files are no longer all
// servable. The version looks present in the list, so without this the failure
// only shows up as a failed install.
function BrokenPublication({ broken, total, repoType }: { broken: number; total: number; repoType: string }) {
  const { t } = useTranslation();
  const advice = repoType === "proxy"
    ? t("repo.artifact-unavailable-proxy")
    : t("repo.artifact-unavailable-hosted");
  return (
    <Tooltip>
      <TooltipTrigger
        render={<span tabIndex={0} className="inline-flex items-center gap-1 text-destructive" aria-label={`${t("upload.publication-broken")} ${advice}`} />}
      >
        <TriangleAlert className="size-3.5" aria-hidden />
        <span className="text-xs tabular-nums">{broken}/{total}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs">
        <span className="flex flex-col gap-1">
          <span className="text-xs font-medium">{t("upload.publication-broken")}</span>
          <span className="text-xs">{advice}</span>
        </span>
      </TooltipContent>
    </Tooltip>
  );
}

export function BrokenArtifact({ repoType, since, lastSeen, lastStatus }: {
  repoType: string; since?: string | null; lastSeen?: string | null; lastStatus?: number;
}) {
  const { t } = useTranslation();
  const advice = repoType === "proxy"
    ? t("repo.artifact-unavailable-proxy")
    : t("repo.artifact-unavailable-hosted");
  return (
    <Tooltip>
      <TooltipTrigger
        render={<span tabIndex={0} className="inline-flex shrink-0" aria-label={`${t("repo.artifact-unavailable")} ${advice}`} />}
      >
        <TriangleAlert className="size-3.5 text-destructive" aria-hidden />
      </TooltipTrigger>
      <TooltipContent className="max-w-xs">
        <span className="flex flex-col gap-1">
          <span className="text-xs font-medium">{t("repo.artifact-unavailable")}</span>
          <span className="text-xs">{advice}</span>
          {lastSeen && (
            <span className="text-xs opacity-75">
              {t("repo.failed-last")} {lastStatus ? `${failedOpLabel(lastStatus, t)} ${lastStatus}, ` : ""}{auditTime(lastSeen)}
            </span>
          )}
          {since && (
            <span className="text-xs opacity-75">{t("repo.artifact-unavailable-since")} {auditTime(since)}</span>
          )}
        </span>
      </TooltipContent>
    </Tooltip>
  );
}

export function Artifacts({ repo, canDelete }: { repo: RepositoryListItem; canDelete: boolean }) {
  const { t } = useTranslation();
  const { me } = useAuth();
  const userIds = useUserIds(Boolean(me.admin || me.auditor));
  const navigate = useNavigate();
  // The action error is separate from the load error: a failed delete must not
  // erase the table it was acting on.
  const [actionError, setActionError] = useState("");
  const queryClient = useQueryClient();
  const [deleting, setDeleting] = useState<Artifact | null>(null);
  const [lifecycle, setLifecycle] = useState<{ publication: ArtifactPublication; action: "delete" | "yank" | "unyank" } | null>(null);
  // The selection is paths rather than rows, so it survives paging and sorting:
  // a repository holds thousands of artifacts and the work of picking them out
  // spans pages. What the reader sees selected on this page is the intersection
  // with the rows currently loaded.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [bulk, setBulk] = useState<"label-add" | "label-remove" | "delete" | null>(null);
  const [actionsOpen, setActionsOpen] = useState(false);
  const [labelDraft, setLabelDraft] = useState("");
  const [bulkBusy, setBulkBusy] = useState(false);
  // A finished batch reports what it did next to what it could not do, which a
  // plain error banner has nowhere to put.
  const [notice, setNotice] = useState("");

  const search = useTableSearch();
  const { highlightRe } = search;

  // Paging or searching replaces the rows the last batch was about, so its
  // report goes with them.
  useEffect(() => {
    setNotice("");
    setActionError("");
  }, [search.q, search.regex, search.page]);

  const artifactsQuery = useQuery({
    ...openApiQueryOptions.listRepositoryArtifacts({
      path: { id: repo.id },
      query: {
        // Empty search and unset regex are omitted rather than sent as "" and
        // false: a present q is a filter as far as the server is concerned.
        q: search.q || undefined,
        regex: search.regex || undefined,
        limit: ARTIFACTS_PAGE_SIZE,
        offset: search.page * ARTIFACTS_PAGE_SIZE,
      },
    }),
    meta: { suppressGlobalErrorToast: true },
  });
  const data = artifactsQuery.data;
  const loadError = getErrorMessageIfAny(artifactsQuery.error);

  const { sorted, sort } = useSort(data?.artifacts ?? [], {
    path: (a) => a.path,
    version: (a) => a.version,
    vuln: (a) => (a.max_severity ? ART_SEV_RANK[a.max_severity] ?? 0 : undefined),
    license: (a) => a.licenses?.join(",") ?? "",
    size: (a) => a.size,
    type: (a) => a.content_type,
    cached: (a) => a.cached_at,
    accessed: (a) => a.last_accessed_at,
    by: (a) => a.cached_by,
    downloads: (a) => a.downloads_30d,
    accessedBy: (a) => a.last_accessed_by,
    labels: (a) => a.labels?.map((l) => l.label).join(",") ?? "",
  });


  // What can be done to a selection, decided from the rows the server sent: it
  // reports the per-artifact label permission, and delete is the repository-wide
  // one this tab was handed.
  // A row can be acted on when it can be deleted or labelled. Labelling is a
  // per-artifact right, so this differs row by row: an uploader owns their own
  // artifacts and nothing else.
  const actionable = (a: Artifact) => canDelete || a.can_label === true;
  const pageActionable = sorted.filter(actionable);
  const pageSelected = sorted.filter((a) => selected.has(a.path));
  // What the menu offers is decided by the selection, not by what happens to be
  // on this page: an uploader's own artifacts can sit fifty pages in, and hiding
  // the control until they are on screen makes a right they hold look like one
  // they do not.
  const offPageSelection = selected.size > pageSelected.length;
  const canLabelSelection = pageSelected.some((a) => a.can_label) || offPageSelection;
  // Removing a label needs a label to remove. Only the rows in hand carry their
  // labels, so a selection reaching onto other pages is undecidable here and the
  // action stays available: the server is the one that knows, and it reports the
  // paths that had nothing to remove.
  const canRemoveLabel = pageSelected.some((a) => (a.labels?.length ?? 0) > 0) || offPageSelection;

  const allPageSelected = pageActionable.length > 0
    && pageActionable.every((a) => selected.has(a.path));
  const selectedPaths = [...selected];

  // The report describes the batch that just ran, so it goes as soon as the
  // reader moves on: a new selection, another page, a different search. Without
  // that it sits there claiming a result for a selection nobody is looking at
  // any more.
  const clearReport = () => {
    setNotice("");
    setActionError("");
  };

  const toggleRow = (path: string, on: boolean) => {
    clearReport();
    setSelected((prev) => {
      const next = new Set(prev);
      if (on) next.add(path);
      else next.delete(path);
      return next;
    });
  };

  // The header checkbox acts on the rows in view, never on the whole
  // repository: selecting what is not on screen would make the count mean
  // something the reader cannot check.
  const togglePage = (on: boolean) => {
    clearReport();
    setSelected((prev) => {
      const next = new Set(prev);
      for (const a of pageActionable) {
        if (on) next.add(a.path);
        else next.delete(a.path);
      }
      return next;
    });
  };

  // Picking an action closes the menu and opens its dialog: leaving the menu
  // standing behind the dialog would put two things asking for the same click on
  // screen at once.
  const openBulk = (action: "label-add" | "label-remove" | "delete") => {
    setActionsOpen(false);
    setBulk(action);
  };

  const closeBulk = () => {
    setBulk(null);
    setLabelDraft("");
  };

  // report renders a finished batch: what changed, and the paths that refused,
  // capped so one bad selection cannot fill the page with its own failures.
  const report = (outcome: ArtifactBulkOutcome) => {
    setNotice(`${t("repo.bulk-succeeded")} ${outcome.succeeded} / ${outcome.requested}`);
    if (outcome.failed.length === 0) {
      setActionError("");
      return;
    }
    const shown = outcome.failed.slice(0, 5).map((f) => `${f.path}: ${f.error}`);
    const rest = outcome.failed.length - shown.length;
    setActionError(
      `${t("repo.bulk-failed")} ${outcome.failed.length}\n` +
        shown.join("\n") +
        (rest > 0 ? `\n${t("repo.bulk-failed-more")} ${rest}` : ""),
    );
  };

  const applyBulk = async () => {
    const paths = selectedPaths;
    if (paths.length === 0) return;
    const label = labelDraft.trim();
    if (bulk !== "delete" && !label) return;
    setBulkBusy(true);
    setActionError("");
    setNotice("");
    try {
      const outcome = bulk === "delete"
        ? await runArtifactBulk(paths, (batch) =>
          postBulkDeleteRepositoryArtifacts({ path: { id: repo.id }, body: { paths: batch } }))
        : await runArtifactBulk(paths, (batch) =>
          postBulkRepositoryArtifactsLabels({
            path: { id: repo.id },
            body: { paths: batch, label, action: bulk === "label-remove" ? "remove" : "add" },
          }));
      report(outcome);
      setSelected(new Set(bulkRetryPaths(paths, outcome)));
      closeBulk();
      await refresh();
    } catch (e) {
      setActionError((e as Error).message);
      closeBulk();
    } finally {
      setBulkBusy(false);
    }
  };

  // Every write here changes what the table and the repository aggregates say,
  // so each refreshes both by prefix rather than one exact page.
  const refresh = () =>
    Promise.all([
      queryClient.invalidateQueries({
        queryKey: operationKeyPrefix(
          openApiQueryKeys.listRepositoryArtifacts({ path: { id: repo.id } }),
        ),
      }),
      queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositories() }),
    ]);

  const del = async (a: Artifact) => {
    setActionError("");
    try {
      await api.deleteArtifact(repo.id, a.path, a.blob_missing === true);
      setDeleting(null);
      await refresh();
    } catch (e) {
      setActionError((e as Error).message);
      setDeleting(null);
    }
  };

  const applyLifecycle = async () => {
    if (!lifecycle) return;
    setActionError("");
    try {
      if (lifecycle.action === "delete") {
        await deleteArtifactPublication(repo.id, lifecycle.publication.id, me.csrf_token);
      } else {
        await setCargoPublicationYanked(repo.id, lifecycle.publication.id, lifecycle.action === "yank", me.csrf_token);
      }
      setLifecycle(null);
      await refresh();
    } catch (e) {
      setActionError((e as Error).message);
      setLifecycle(null);
    }
  };

  return (
    <Card size="sm" className="mb-4">
      <CardContent>
      <div className="mb-4 flex items-start justify-between gap-3 max-sm:flex-col max-sm:items-stretch">
        <div className="flex min-w-0 flex-wrap items-baseline gap-3">
          <h2 className="m-0 text-base font-semibold">{t("common.artifacts")}</h2>
          {data && <span className="text-sm text-muted-foreground">{data.count} items · {formatFileSize(data.total_size)}</span>}
        </div>
        <div className="flex flex-wrap items-center gap-2 max-sm:w-full">
          {/* One control for everything that acts on a selection, rather than a
              button per action: the bar is read at a glance and stays the same
              width whatever is selected. It is enabled only with a selection,
              because every item inside it needs one. */}
          <Popover open={actionsOpen} onOpenChange={setActionsOpen}>
              <PopoverTrigger
                render={
                  <Button
                    type="button"
                    variant="outline"
                    size="lg"
                    // Wider than its label needs: it is the one control here
                    // that is aimed at repeatedly, so it gets the target area.
                    //
                    // Disabled says so in more than opacity: an outline button at
                    // half strength still reads as a button on a panel this
                    // light. It loses its fill and its border weight, keeps the
                    // pointer events its base class drops so the cursor can
                    // refuse, and says what is missing on hover.
                    className={cn(
                      "px-4",
                      "disabled:pointer-events-auto disabled:cursor-not-allowed disabled:border-[var(--fx-border-subtle)]",
                      "disabled:bg-transparent disabled:text-muted-foreground disabled:opacity-100",
                    )}
                    title={selected.size === 0 ? t("repo.bulk-none-selected") : undefined}
                    disabled={selected.size === 0 || bulkBusy}
                  />
                }
              >
                {t("repo.bulk-actions")}
                <ChevronDown className="size-4" aria-hidden="true" />
              </PopoverTrigger>
              {/* A menu, not a panel: the width follows the longest item and the
                  padding is the item's own, so the box is the list and nothing
                  else. */}
              <PopoverContent align="end" className="w-auto min-w-0 gap-0 p-1">
                {/* Every action stays on the list, disabled with its reason,
                    so the menu is the same menu for everyone and a reader learns
                    what the tab can do rather than only what they may do. The
                    menu itself is still hidden from someone who may do none of
                    it: a control that can never enable is not information. */}
                <MenuItem
                  disabled={!canLabelSelection}
                  title={canLabelSelection ? undefined : t("repo.bulk-label-denied")}
                  onClick={() => openBulk("label-add")}
                >
                  {t("repo.bulk-add-label")}
                </MenuItem>
                <MenuItem
                  disabled={!canLabelSelection || !canRemoveLabel}
                  title={!canLabelSelection
                    ? t("repo.bulk-label-denied")
                    : canRemoveLabel ? undefined : t("repo.bulk-remove-label-none")}
                  onClick={() => openBulk("label-remove")}
                >
                  {t("repo.bulk-remove-label")}
                </MenuItem>
                <MenuItem
                  variant="danger"
                  disabled={!canDelete}
                  title={canDelete ? undefined : t("repo.bulk-delete-denied")}
                  onClick={() => openBulk("delete")}
                >
                  {t("repo.bulk-delete")}
                </MenuItem>
            </PopoverContent>
          </Popover>
          {repo.type === "hosted" && (repo.capabilities?.upload || repo.can_write) && (
            <Button type="button" className="max-sm:w-full"
              onClick={() => navigate({ to: "/workspace/repositories/$id/upload", params: { id: String(repo.id) } })}>
              <Upload className="size-4" aria-hidden="true" />
              {t("repo.upload-action")}
            </Button>
          )}
        </div>
      </div>
      <TableSearchControls search={search} className="mb-4" />
      {search.regexError && <Alert className="mb-4">{t("common.invalid-regex")}</Alert>}
      {notice && (
        <div className="mb-4 flex items-start justify-between gap-3 rounded-md border border-accent-ink/40 bg-primary/10 px-3 py-2 text-sm">
          <span className="min-w-0">{notice}</span>
          <button
            type="button"
            aria-label={t("common.close")}
            className="flex size-6 shrink-0 cursor-pointer items-center justify-center rounded-md border border-transparent text-muted-foreground transition-colors hover:bg-[var(--fx-surface-hover)] hover:text-foreground"
            onClick={clearReport}
          >
            <X className="size-4" aria-hidden="true" />
          </button>
        </div>
      )}
      {(actionError || loadError) && (
        <Alert className="mb-4"><span className="whitespace-pre-line">{actionError || loadError}</span></Alert>
      )}
      {data && data.publications.length > 0 && (
        <div className="mb-6">
          <h3 className="mb-2 text-sm font-semibold">{t("upload.publications")}</h3>
          <TableWrap>
            <Table>
              <TableHeader><TableRow><TableHead>{t("common.package")}</TableHead><TableHead>{t("common.version")}</TableHead><TableHead>{t("common.status")}</TableHead><TableHead>{t("common.size")}</TableHead><TableHead>{t("upload.published-at")}</TableHead><TableHead>{t("upload.published-by")}</TableHead><TableHead>{t("common.actions")}</TableHead></TableRow></TableHeader>
              <TableBody>{data.publications.map((publication) => (
                <TableRow key={publication.id}>
                  <TableCell>
                    <CopyOnHover value={publication.coordinate}>
                      <span className="min-w-0 break-all font-medium">{publication.coordinate}</span>
                    </CopyOnHover>
                    <div className="text-xs text-muted-foreground">{publication.asset_count} paths</div>
                  </TableCell>
                  <TableCell>{publication.version}</TableCell>
                  <TableCell>
                    <div className="flex flex-wrap items-center gap-1.5">
                      {(publication.broken_assets ?? 0) > 0 && (
                        <BrokenPublication
                          broken={publication.broken_assets ?? 0}
                          total={publication.asset_count}
                          repoType={repo.type}
                        />
                      )}
                      {publication.yanked && <Badge variant="outline">{t("upload.yanked")}</Badge>}
                      {(publication.broken_assets ?? 0) === 0 && !publication.yanked && (
                        <span className="text-muted-foreground">-</span>
                      )}
                    </div>
                  </TableCell>
                  <TableCell>{formatFileSize(publication.total_size)}</TableCell>
                  <TableCell className="whitespace-nowrap text-muted-foreground">
                    {artifactTime(publication.created_at)}
                    {publication.updated_at !== publication.created_at && (
                      <div className="text-xs">{t("upload.updated-at")} {artifactTime(publication.updated_at)}</div>
                    )}
                  </TableCell>
                  <TableCell className="truncate" title={publication.created_by || t("common.anonymous")}>
                    {publication.created_by
                      ? userLink(publication.created_by, userIds)
                      : <span className="text-muted-foreground">{t("common.anonymous")}</span>}
                  </TableCell>
                  <TableCell><div className="flex flex-wrap gap-2">
                    {publication.actions.includes("replace") && <Link className={buttonVariants({ variant: "outline", size: "sm" })} to="/workspace/repositories/$id/upload" params={{ id: String(repo.id) }}>{t("upload.replace")}</Link>}
                    {publication.actions.includes("extend") && <Link className={buttonVariants({ variant: "outline", size: "sm" })} to="/workspace/repositories/$id/upload" params={{ id: String(repo.id) }}>{t("upload.extend")}</Link>}
                    {publication.actions.includes("delete") && <Button size="sm" variant="destructive" onClick={() => setLifecycle({ publication, action: "delete" })}>{t("common.delete")}</Button>}
                    {publication.actions.includes("yank") && <Button size="sm" variant="outline" onClick={() => setLifecycle({ publication, action: "yank" })}>{t("upload.yank")}</Button>}
                    {publication.actions.includes("unyank") && <Button size="sm" variant="outline" onClick={() => setLifecycle({ publication, action: "unyank" })}>{t("upload.unyank")}</Button>}
                  </div></TableCell>
                </TableRow>
              ))}</TableBody>
            </Table>
          </TableWrap>
        </div>
      )}
      <TableWrap>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className="w-6 pr-0">
              <Checkbox
                aria-label={t("repo.bulk-select-page")}
                checked={allPageSelected}
                indeterminate={pageSelected.length > 0 && !allPageSelected}
                disabled={pageActionable.length === 0 || bulkBusy}
                onCheckedChange={(checked) => togglePage(Boolean(checked))}
              />
            </TableHead><SortableHead k="path" sort={sort}>{t("common.path")}</SortableHead><SortableHead k="version" sort={sort}>{t("common.version")}</SortableHead><SortableHead k="labels" sort={sort}>{t("common.labels")}</SortableHead><SortableHead k="vuln" sort={sort}>{t("common.vuln")}</SortableHead><SortableHead k="license" sort={sort}>{t("common.license")}</SortableHead><SortableHead k="size" sort={sort}>{t("common.size")}</SortableHead><SortableHead k="type" sort={sort}>{t("common.type")}</SortableHead><SortableHead k="cached" sort={sort}>{t("common.first-cached")}</SortableHead><SortableHead k="downloads" sort={sort}><span title={t("repo.downloads-30d-help")}>{t("repo.downloads-30d")}</span></SortableHead><SortableHead k="accessed" sort={sort}>{t("common.last-accessed")}</SortableHead><SortableHead k="accessedBy" sort={sort}><span title={t("repo.last-accessed-by-help")}>{t("repo.last-accessed-by")}</span></SortableHead><SortableHead k="by" sort={sort}>{t("common.fetched-by")}</SortableHead>{canDelete && <TableHead></TableHead>}</TableRow>
        </TableHeader>
        <TableBody>
          {sorted.map((a) => (
            <TableRow key={a.path} data-state={selected.has(a.path) ? "selected" : undefined}>
              {/* A row nobody may act on is not selectable, and says so with a
                  different shape rather than a faded checkbox: down fifty rows, a
                  disabled box reads as one that has not been ticked yet. */}
              <TableCell className="w-6 pr-0 align-top">
                {actionable(a) ? (
                  <Checkbox
                    aria-label={`${t("repo.bulk-select-row")} ${a.path}`}
                    checked={selected.has(a.path)}
                    disabled={bulkBusy}
                    onCheckedChange={(checked) => toggleRow(a.path, Boolean(checked))}
                  />
                ) : (
                  <span
                    className="flex size-4 items-center justify-center text-muted-foreground/70"
                    aria-label={t("repo.bulk-row-denied")}
                  >
                    <Lock className="size-3.5" aria-hidden="true" />
                  </span>
                )}
              </TableCell>
              <TableCell className="break-all font-mono text-xs">
                <span className="flex items-start gap-1.5">
                  {a.blob_missing && <BrokenArtifact repoType={repo.type} since={a.blob_missing_since} lastSeen={a.blob_missing_last_seen} lastStatus={a.blob_missing_last_status} />}
                  <CopyOnHover value={a.path} className="items-start">
                    <span className="break-all">{highlightMatches(a.path, highlightRe)}</span>
                  </CopyOnHover>
                </span>
              </TableCell>
              <TableCell>{a.version ? highlightMatches(a.version, highlightRe) : "-"}</TableCell>
              <TableCell>
                <ArtifactLabels
                  repoId={repo.id}
                  path={a.path}
                  labels={a.labels ?? []}
                  canLabel={a.can_label ?? false}
                  highlightRe={highlightRe}
                  onError={setActionError}
                />
              </TableCell>
              <TableCell><SeverityBar severity={a.max_severity} counts={a.vuln_counts} source={a.vuln_source} scannedAt={a.vuln_scanned_at} advisories={a.vuln_advisories} /></TableCell>
              <TableCell>
                {a.licenses && a.licenses.length > 0 ? (
                  <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                    {a.licenses.map((licenseId) => <Badge key={licenseId} variant="outline">{highlightMatches(licenseId, highlightRe)}</Badge>)}
                  </div>
                ) : (
                  <span className="text-muted-foreground">-</span>
                )}
              </TableCell>
              <TableCell className="text-muted-foreground">{highlightMatches(formatFileSize(a.size), highlightRe)}</TableCell>
              <TableCell className="text-muted-foreground">{highlightMatches(a.content_type, highlightRe)}</TableCell>
              <TableCell className="text-muted-foreground">{highlightMatches(artifactTime(a.cached_at), highlightRe)}</TableCell>
              <TableCell className="text-muted-foreground tabular-nums">{a.downloads_30d.toLocaleString()}</TableCell>
              <TableCell className="text-muted-foreground">{highlightMatches(artifactTime(a.last_accessed_at), highlightRe)}</TableCell>
              <TableCell className="truncate" title={a.last_accessed_by || t("repo.last-accessed-by-unknown")}>
                {a.last_accessed_by
                  ? userLink(a.last_accessed_by, userIds, highlightMatches(a.last_accessed_by, highlightRe))
                  : <span className="text-muted-foreground">-</span>}
              </TableCell>
              <TableCell className="truncate" title={a.cached_by || t("common.anonymous")}>{a.cached_by ? userLink(a.cached_by, userIds, highlightMatches(a.cached_by, highlightRe)) : <span className="text-muted-foreground">{t("common.anonymous")}</span>}</TableCell>
              {canDelete && (
                // Deleting is a bulk action now, with one exception that cannot
                // be one: an artifact whose bytes are gone is removed by force,
                // which is allowed only on proof that they are gone, one blob
                // store round trip each. That stays on the row it repairs.
                <TableCell className="text-right">
                  {a.blob_missing && (
                    <Button variant="outline" onClick={() => setDeleting(a)}>{t("repo.force-delete")}</Button>
                  )}
                </TableCell>
              )}
            </TableRow>
          ))}
          {data && data.artifacts.length === 0 && (
            <TableRow><TableCell colSpan={13 + (canDelete ? 1 : 0)} className="text-muted-foreground">
              {search.q ? t("common.no-search-matches") : t("repo.no-cached-artifacts")}
            </TableCell></TableRow>
          )}
        </TableBody>
      </Table>
      </TableWrap>
      {/* The selection count belongs next to the range, not in a bar of its own:
          both answer "how much am I looking at", and a selection spans pages, so
          it reads against the range it is a subset of. */}
      <TablePager
        page={search.page}
        pageSize={ARTIFACTS_PAGE_SIZE}
        total={data?.filtered ?? 0}
        onPage={search.setPage}
        note={selected.size > 0 ? (
          <span className="flex min-w-0 items-center gap-2 text-sm">
            <span className="tabular-nums">{selected.size.toLocaleString()} {t("repo.bulk-selected")}</span>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => { clearReport(); setSelected(new Set()); }}
              disabled={bulkBusy}
            >
              {t("repo.bulk-clear")}
            </Button>
            {bulkBusy && <span className="text-muted-foreground">{t("repo.bulk-running")}</span>}
          </span>
        ) : undefined}
      />
      <ConfirmModal
        open={deleting !== null}
        title="Delete artifact"
        message={deleting
          ? deleting.blob_missing
            ? `${deleting.path} ${t("repo.force-delete-help")}`
            : `${deleting.path} will be removed from this repository. A proxy can re-cache it on the next request; a hosted upload cannot be recovered.`
          : undefined}
        confirmLabel={t("common.delete")}
        danger
        onConfirm={() => deleting && del(deleting)}
        onCancel={() => setDeleting(null)}
      />
      {/* The label modals are their own dialog rather than the shared confirm
          one: they take a value, and the value is the whole decision. */}
      <AlertDialog open={bulk === "label-add" || bulk === "label-remove"} onOpenChange={(next) => { if (!next) closeBulk(); }}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {bulk === "label-remove" ? t("repo.bulk-remove-label") : t("repo.bulk-add-label")}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {selected.size} {t("repo.bulk-selected")}. {t("repo.bulk-label-help")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <Input
            autoFocus
            value={labelDraft}
            placeholder={t("label.placeholder")}
            onChange={(e) => setLabelDraft(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter" && labelDraft.trim()) void applyBulk(); }}
          />
          <AlertDialogFooter>
            <AlertDialogCancel disabled={bulkBusy}>{t("common.cancel")}</AlertDialogCancel>
            <AlertDialogAction disabled={bulkBusy || !labelDraft.trim()} onClick={() => void applyBulk()}>
              {bulk === "label-remove" ? t("common.remove") : t("common.add")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <ConfirmModal
        open={bulk === "delete"}
        title={t("repo.bulk-delete")}
        message={`${selected.size} ${t("repo.bulk-selected")}. ${t("repo.bulk-delete-help")}`}
        confirmLabel={t("common.delete")}
        danger
        // Typing the word is the guard. It stays "delete" in every language: the
        // point is a deliberate second act before removing this much at once,
        // and a word that changes with the interface language is one more thing
        // to get wrong under a confirmation prompt.
        confirmText="delete"
        onConfirm={() => void applyBulk()}
        onCancel={closeBulk}
      />
      <ConfirmModal
        open={lifecycle !== null}
        title={lifecycle ? `${lifecycle.action} ${lifecycle.publication.coordinate}` : ""}
        message={lifecycle ? t(lifecycle.action === "delete" ? "upload.delete-publication-help" : "upload.yank-help") : undefined}
        confirmLabel={lifecycle?.action === "delete" ? t("common.delete") : lifecycle?.action}
        danger={lifecycle?.action === "delete"}
        onConfirm={applyLifecycle}
        onCancel={() => setLifecycle(null)}
      />
      </CardContent>
    </Card>
  );
}
