import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { ArtifactLabels } from "@/components/app-ui/artifact-labels";
import { Badge } from "@/components/app-ui/badge";
import { CopyOnHover } from "@/components/app-ui/copy-button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { ChevronDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { MenuItem } from "@/components/ui/menu-item";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { bulkRetryPaths, runArtifactBulk } from "@/lib/artifact-bulk";
import { postBulkRepositoryArtifactsLabels } from "@/services/v1/repositories/api";
import { Card, CardContent } from "@/components/ui/card";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { formatFileSize } from "@/utils/format-file-size";

import type { OCITagInfo, Repository } from "@/services/v1/openapi-types";

// OCIImagesTab is the Harbor-style artifact view for OCI repositories: one row
// per tag, with the image name, kind, platforms, content size, digest and push
// time, plus a copyable pull reference. The raw blob/manifest artifact rows
// stay reachable through the API; this view is what an operator thinks in.
export function OCIImagesTab({ repo }: { repo: Repository }) {
  const { t } = useTranslation();
  const formatDate = useDateTime();
  const navigate = useNavigate();
  // A label write reports here rather than through the query: it is the user's
  // own action failing, not the listing.
  const [actionError, setActionError] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [actionsOpen, setActionsOpen] = useState(false);
  const [action, setAction] = useState<"add" | "remove" | null>(null);
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const queryClient = useQueryClient();

  const tagsQuery = useQuery({
    ...openApiQueryOptions.listRepositoryOciTags({ path: { id: repo.id } }),
    meta: { suppressGlobalErrorToast: true },
  });

  const open = (row: OCITagInfo) =>
    navigate({
      to: "/workspace/repositories/$id/oci-artifact",
      params: { id: String(repo.id) },
      search: { name: row.name, ref: row.tag || row.digest },
    });

  const error = getErrorMessageIfAny(tagsQuery.error);
  if (error) return <Alert className="my-2.5">{error}</Alert>;
  if (tagsQuery.isPending) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;

  const tags = tagsQuery.data?.tags ?? [];
  const host = window.location.host;
  // Several tags may refer to one manifest. Mutate each authorized path once.
  const labelable = [...new Set(tags.filter((row) => row.can_label).map((row) => row.path))];
  const paths = labelable.filter((path) => selected.has(path));
  const allSelected = labelable.length > 0 && paths.length === labelable.length;
  const toggle = (path: string, checked: boolean) => {
    setNotice("");
    setSelected((previous) => {
      const next = new Set(previous);
      if (checked) next.add(path);
      else next.delete(path);
      return next;
    });
  };
  const openAction = (next: "add" | "remove") => {
    setActionsOpen(false);
    setLabel("");
    setAction(next);
  };
  const apply = async () => {
    if (busy || !action || !label.trim() || paths.length === 0) return;
    setBusy(true);
    setActionError("");
    setNotice("");
    try {
      const result = await runArtifactBulk(paths, (batch) => postBulkRepositoryArtifactsLabels({
        path: { id: repo.id }, body: { paths: batch, label: label.trim(), action },
      }));
      setSelected(new Set(bulkRetryPaths(paths, result)));
      setNotice(`${t("repo.bulk-succeeded")} ${result.succeeded} / ${result.requested}`);
      if (result.failed.length) {
        setActionError(`${t("repo.bulk-failed")} ${result.failed.length}\n${result.failed.slice(0, 5).map((failure) => `${failure.path}: ${failure.error}`).join("\n")}`);
      }
      setAction(null);
      await queryClient.invalidateQueries({ queryKey: openApiQueryKeys.listRepositoryOciTags({ path: { id: repo.id } }) });
    } catch (error) {
      setActionError((error as Error).message);
      setAction(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card size="sm" className="mb-4">
      <CardContent>
      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <h2 className="m-0 text-base font-semibold">
          {t("oci.images")} <span className="text-sm font-normal text-muted-foreground">{tags.length}</span>
        </h2>
        <Popover open={actionsOpen} onOpenChange={setActionsOpen}>
          <PopoverTrigger render={<Button variant="outline" disabled={paths.length === 0 || busy} />}>
            {t("repo.bulk-actions")} <ChevronDown className="size-4" aria-hidden="true" />
          </PopoverTrigger>
          <PopoverContent align="end" className="w-auto min-w-0 gap-0 p-1">
            <MenuItem onClick={() => openAction("add")}>{t("repo.bulk-add-label")}</MenuItem>
            <MenuItem onClick={() => openAction("remove")}>{t("repo.bulk-remove-label")}</MenuItem>
          </PopoverContent>
        </Popover>
      </div>
      {paths.length > 0 && <p className="mb-2 text-sm text-muted-foreground">{paths.length} {t("repo.bulk-selected")}</p>}
      {notice && <p role="status" className="mb-2 text-sm text-muted-foreground">{notice}</p>}
      {actionError && <Alert className="my-2.5 whitespace-pre-line">{actionError}</Alert>}
      {tags.length === 0 ? (
        <p className="mt-0 mb-0 text-sm text-muted-foreground">{t("oci.no-images")}</p>
      ) : (
        <TableWrap>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-6 pr-0">
                  <Checkbox aria-label={t("repo.bulk-select-page")} checked={allSelected}
                    indeterminate={paths.length > 0 && !allSelected} disabled={labelable.length === 0 || busy}
                    onCheckedChange={(checked) => { setNotice(""); setSelected(new Set(checked ? labelable : [])); }} />
                </TableHead>
                <TableHead>{t("oci.image")}</TableHead>
                <TableHead>{t("oci.tag")}</TableHead>
                <TableHead>{t("common.labels")}</TableHead>
                <TableHead>{t("common.type")}</TableHead>
                <TableHead>{t("oci.platforms")}</TableHead>
                <TableHead>{t("common.size")}</TableHead>
                <TableHead>{t("oci.digest")}</TableHead>
                <TableHead>{t("oci.push-time")}</TableHead>
                <TableHead>{t("oci.pushed-by")}</TableHead>
                <TableHead>{t("oci.pull-time")}</TableHead>
                <TableHead>{t("oci.pulled-by")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {tags.map((row) => (
                <TableRow
                  key={`${row.name}:${row.tag}`}
                  className="cursor-pointer"
                  data-state={selected.has(row.path) ? "selected" : undefined}
                  onClick={() => open(row)}
                >
                  <TableCell className="w-6 pr-0" onClick={(event) => event.stopPropagation()}>
                    <Checkbox aria-label={`${t("repo.bulk-select-row")} ${row.name}:${row.tag || row.digest}`}
                      checked={selected.has(row.path)} disabled={!row.can_label || busy}
                      onCheckedChange={(checked) => toggle(row.path, Boolean(checked))} />
                  </TableCell>
                  <TableCell className="font-mono text-xs" onClick={(e) => e.stopPropagation()}>
                    <CopyOnHover value={`${repo.name}/${row.name}`}>
                      <span className="min-w-0 break-all">{row.name}</span>
                    </CopyOnHover>
                  </TableCell>
                  <TableCell className="whitespace-nowrap" onClick={(e) => e.stopPropagation()}>
                    <CopyOnHover value={`${host}/${repo.name}/${row.name}:${row.tag}`}>
                      <Badge variant="outline" className="cursor-pointer" onClick={() => open(row)}>{row.tag}</Badge>
                    </CopyOnHover>
                  </TableCell>
                  <TableCell onClick={(e) => e.stopPropagation()}>
                    <ArtifactLabels
                      repoId={repo.id}
                      path={row.path}
                      labels={row.labels ?? []}
                      canLabel={row.can_label ?? false}
                      onError={setActionError}
                    />
                  </TableCell>
                  <TableCell className="text-muted-foreground">{row.kind}</TableCell>
                  <TableCell className="text-muted-foreground">
                    {row.platforms.length > 0 ? row.platforms.join(", ") : "-"}
                  </TableCell>
                  <TableCell className="whitespace-nowrap">{formatFileSize(row.size)}</TableCell>
                  <TableCell className="whitespace-nowrap font-mono text-xs" title={row.digest} onClick={(e) => e.stopPropagation()}>
                    <CopyOnHover value={row.digest}>{row.digest.slice(7, 19)}</CopyOnHover>
                  </TableCell>
                  <TableCell className="whitespace-nowrap text-muted-foreground">{formatDate(row.pushed_at)}</TableCell>
                  <TableCell className="whitespace-nowrap text-muted-foreground">{row.pushed_by || "-"}</TableCell>
                  <TableCell className="whitespace-nowrap text-muted-foreground">{row.pulled_at ? formatDate(row.pulled_at) : "-"}</TableCell>
                  <TableCell className="whitespace-nowrap text-muted-foreground">{row.pulled_by || "-"}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </TableWrap>
      )}
      <AlertDialog open={action !== null} onOpenChange={(open) => { if (!open && !busy) setAction(null); }}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{action === "remove" ? t("repo.bulk-remove-label") : t("repo.bulk-add-label")}</AlertDialogTitle>
            <AlertDialogDescription>{paths.length} {t("repo.bulk-selected")}. {t("repo.bulk-label-help")}</AlertDialogDescription>
          </AlertDialogHeader>
          <Input autoFocus value={label} disabled={busy} placeholder={t("label.placeholder")}
            onChange={(event) => setLabel(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Enter") void apply(); }} />
          <AlertDialogFooter>
            <AlertDialogCancel disabled={busy}>{t("common.cancel")}</AlertDialogCancel>
            <AlertDialogAction disabled={busy || !label.trim()} onClick={() => void apply()}>
              {action === "remove" ? t("common.remove") : t("common.add")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      </CardContent>
    </Card>
  );
}
