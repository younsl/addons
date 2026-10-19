import { useInfiniteQuery } from "@tanstack/react-query";
import { useState } from "react";

import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableWrap } from "@/components/app-ui/table";
import { Button } from "@/components/ui/button";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useDateTime, useTranslation, type MessageKey } from "@/lib/i18n";
import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { getInsightCheckAffected } from "@/services/v1/insights/api";
import type { InsightFinding } from "@/services/v1/openapi-types";
import { AffectedLink } from "./finding-card";

const PAGE = 100;

// The report carries the first rows of each finding. Show more pages in the
// rest from the same run, so a newer run resets the list. Loaded pages are not
// cached: leaving the page collapses the list back to the first rows.
export function FlaggedItems({ finding, generatedAt }: { finding: InsightFinding; generatedAt: string }) {
  const { t } = useTranslation();
  const formatDate = useDateTime();
  const [expanded, setExpanded] = useState(false);
  const more = useInfiniteQuery({
    queryKey: [...openApiQueryKeys.getInsightCheckAffected({ path: { id: finding.id } }), generatedAt],
    queryFn: ({ pageParam, signal }) =>
      getInsightCheckAffected({ path: { id: finding.id }, query: { offset: pageParam, limit: PAGE } }, { signal }),
    initialPageParam: finding.affected.length,
    getNextPageParam: (last, _pages, offset) => {
      const next = offset + last.items.length;
      return last.items.length > 0 && next < last.listed ? next : undefined;
    },
    enabled: expanded,
    gcTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  const pages = expanded ? (more.data?.pages ?? []) : [];
  const listed = pages.length > 0 ? pages[pages.length - 1].listed : finding.total;
  const items = [...finding.affected, ...pages.flatMap((p) => p.items)];
  const canLoad = pages.length > 0 ? more.hasNextPage : items.length < listed;
  const error = getErrorMessageIfAny(more.error);
  // Roles belong to no repository, so their rows show who holds them and when
  // they were made instead.
  const roles = finding.affected.length > 0 && finding.affected.every((a) => a.kind === "role");

  return (
    <>
      <TableWrap>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-px pr-4 text-right">#</TableHead>
              <TableHead className="w-px pr-4">{t("insights.resource-type")}</TableHead>
              <TableHead>{t("insights.affected")}</TableHead>
              {roles ? (
                <>
                  <TableHead className="w-px pr-4 text-right">{t("insights.role-users")}</TableHead>
                  <TableHead className="w-px pr-4">{t("common.created")}</TableHead>
                </>
              ) : (
                <TableHead className="w-px pr-4">{t("common.repository")}</TableHead>
              )}
              <TableHead>{t("insights.detail")}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {items.map((item, i) => (
              <TableRow key={`${item.kind}:${item.label}:${item.repo ?? ""}:${i}`}>
                <TableCell className="pr-4 text-right tabular-nums text-muted-foreground">{i + 1}</TableCell>
                <TableCell className="whitespace-nowrap pr-4 text-muted-foreground">{t(`insights.kind-${item.kind}` as MessageKey)}</TableCell>
                <TableCell className="min-w-48 whitespace-normal break-all font-mono text-xs"><AffectedLink finding={finding} item={item} /></TableCell>
                {roles ? (
                  <>
                    <TableCell className="pr-4 text-right tabular-nums text-muted-foreground">{item.user_count ?? "-"}</TableCell>
                    <TableCell className="whitespace-nowrap pr-4 text-muted-foreground">{formatDate(item.created_at) || "-"}</TableCell>
                  </>
                ) : (
                  <TableCell className="pr-4 text-muted-foreground">{item.repo || "-"}</TableCell>
                )}
                <TableCell className="text-muted-foreground">{item.detail || "-"}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </TableWrap>
      {items.length < finding.total && (
        <div className="mt-2 flex flex-wrap items-center gap-3">
          <p className="m-0 text-xs text-muted-foreground">
            {t("insights.showing-items").replace("{shown}", items.length.toLocaleString()).replace("{total}", finding.total.toLocaleString())}
          </p>
          {canLoad && (
            <Button
              variant="outline"
              size="sm"
              disabled={more.isFetching}
              onClick={() => {
                if (!expanded) setExpanded(true);
                else if (pages.length > 0) void more.fetchNextPage();
                else void more.refetch();
              }}
            >
              {more.isFetching ? t("common.loading") : t("insights.show-more")}
            </Button>
          )}
          {error && <p className="m-0 text-xs text-destructive">{error}</p>}
        </div>
      )}
    </>
  );
}
