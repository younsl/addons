import { useEffect, useState } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import {
  SortableHead,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
} from "@/components/app-ui/table";
import { useAuth } from "@/authContext";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { AddRuleModal } from "@/routes/workspace/approvals/-components/add-rule-modal";
import { APPROVALS_PAGE_SIZE } from "@/routes/workspace/approvals/-hooks/use-approval-queue";
import { useRemoveVersionDenyMutation } from "@/routes/workspace/approvals/-hooks/use-approval-mutations";
import { useVersionDenies } from "@/routes/workspace/approvals/-hooks/use-version-denies";
import { repoLink } from "@/routes/workspace/approvals/-utils/entity-links";
import { canReviewApprovals } from "@/utils/permissions";

import type { VersionDeny } from "@/services/v1/openapi-types";

// VersionDenies is the per-version deny list: the package stays approved while
// single poisoned releases are cut off (incident response, IOC blocking). The
// deny overrides package approval and blocks already-cached copies immediately.
export function VersionDenies({
  repo = "",
  showRepo = true,
  showAdd = true,
  repoNames,
  repoIds = {},
  embedded = false,
}: {
  repo?: string;
  showRepo?: boolean;
  // Hidden where a page-level "Add rule" button already covers adding a deny.
  showAdd?: boolean;
  repoNames: string[];
  // Maps repository name to id so the Repository column can link to its detail.
  repoIds?: Record<string, number>;
  embedded?: boolean;
}) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const { me } = useAuth();
  // Version denies are mutations; auditors see the list but cannot add or remove.
  const canDecide = canReviewApprovals(me);
  const [offset, setOffset] = useState(0);
  const [isAdding, setIsAdding] = useState(false);
  const [removing, setRemoving] = useState<VersionDeny | null>(null);
  const [actionError, setActionError] = useState("");
  const { count, error: loadError, rows } = useVersionDenies({ repo, offset });
  const removeMutation = useRemoveVersionDenyMutation();
  const { sorted, sort } = useSort(rows, {
    repo: (deny) => deny.repo_name,
    package: (deny) => deny.package,
    version: (deny) => deny.version,
    reason: (deny) => deny.reason,
    deniedBy: (deny) => deny.created_by,
    deniedAt: (deny) => deny.created_at,
  });

  // A new repository scope means a different list; page 3 of it may not exist.
  useEffect(() => { setOffset(0); }, [repo]);

  const error = actionError || loadError;

  const remove = (deny: VersionDeny) => {
    setActionError("");
    removeMutation.mutate(deny.id, {
      onSuccess: () => setRemoving(null),
      onError: (caught) => setActionError(getErrorMessage(caught)),
    });
  };

  return (
    <section aria-labelledby="version-denies-title">
      <div
        className={cn(
          "mb-3 flex items-end justify-between gap-3 max-sm:flex-col max-sm:items-stretch",
          !embedded && "border-t border-[var(--fx-border-subtle)] pt-7",
        )}
      >
        <div>
          <h2 id="version-denies-title" className="m-0 text-base font-semibold">
            {t("approval.version-denies")}
          </h2>
          <p className="m-0 mt-1 text-sm leading-6 text-muted-foreground">
            {t("approval.deny-version-hint")}
          </p>
        </div>
        {canDecide && showAdd && (
          <Button className="shrink-0" variant="destructive" onClick={() => setIsAdding(true)}>
            {t("approval.deny-version")}
          </Button>
        )}
      </div>
      {error && <Alert className="mb-4">{error}</Alert>}
      {rows.length === 0 ? (
        <div className="rounded-md border border-dashed border-[var(--fx-border-subtle)] px-3 py-8 text-center text-sm text-muted-foreground">
          {t("approval.no-denied")}
        </div>
      ) : (
        <TableWrap>
          <Table className="min-w-[860px] table-fixed">
            <TableHeader>
              <TableRow>
                {showRepo && (
                  <SortableHead k="repo" sort={sort} className="w-[14%]">
                    {t("common.repository")}
                  </SortableHead>
                )}
                <SortableHead k="package" sort={sort} className="w-[24%]">{t("common.package")}</SortableHead>
                <SortableHead k="version" sort={sort} className="w-[12%]">{t("common.version")}</SortableHead>
                <SortableHead k="reason" sort={sort}>{t("common.reason")}</SortableHead>
                <SortableHead k="deniedBy" sort={sort} className="w-[12%]">{t("approval.denied-by")}</SortableHead>
                <SortableHead k="deniedAt" sort={sort} className="w-[16%]">{t("approval.denied-at")}</SortableHead>
                <TableHead className="w-[76px] text-right"></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sorted.map((deny) => (
                <TableRow key={deny.id} data-testid={`row-${deny.package}@${deny.version}`}>
                  {showRepo && (
                    <TableCell className="truncate" title={deny.repo_name}>
                      {repoLink(deny.repo_name, repoIds)}
                    </TableCell>
                  )}
                  <TableCell className="max-w-[260px] truncate font-mono text-xs" title={deny.package}>
                    {deny.package}
                  </TableCell>
                  <TableCell className="font-mono text-xs">{deny.version}</TableCell>
                  <TableCell className="truncate" title={deny.reason || t("common.none")}>
                    {deny.reason || <span className="text-muted-foreground">{t("common.none")}</span>}
                  </TableCell>
                  <TableCell className="truncate" title={deny.created_by || t("common.unknown")}>
                    {deny.created_by || <span className="text-muted-foreground">{t("common.unknown")}</span>}
                  </TableCell>
                  <TableCell className="truncate text-muted-foreground" title={fmtDate(deny.created_at)}>
                    {fmtDate(deny.created_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    {canDecide && (
                      <Button variant="outline" onClick={() => setRemoving(deny)}>
                        {t("common.remove")}
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </TableWrap>
      )}
      {count > APPROVALS_PAGE_SIZE && (
        <div className="mt-3 flex min-w-0 items-center gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
          <Button
            variant="outline"
            disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - APPROVALS_PAGE_SIZE))}
          >
            {t("common.newer")}
          </Button>
          <Button
            variant="outline"
            disabled={offset + APPROVALS_PAGE_SIZE >= count}
            onClick={() => setOffset(offset + APPROVALS_PAGE_SIZE)}
          >
            {t("common.older")}
          </Button>
          <span className="text-sm text-muted-foreground">
            {offset + 1}–{Math.min(offset + APPROVALS_PAGE_SIZE, count)} of {count}
          </span>
        </div>
      )}
      {isAdding && (
        <AddRuleModal
          repoNames={repoNames}
          initialRepo={repo}
          initialDecision="block"
          onDone={() => setIsAdding(false)}
          onCancel={() => setIsAdding(false)}
        />
      )}
      <ConfirmModal
        open={removing !== null}
        title={t("approval.remove-deny")}
        message={removing
          ? `${removing.package}@${removing.version} on ${removing.repo_name} will be served again (approval and age policies still apply).`
          : undefined}
        confirmLabel={t("common.remove")}
        onConfirm={() => removing && remove(removing)}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
