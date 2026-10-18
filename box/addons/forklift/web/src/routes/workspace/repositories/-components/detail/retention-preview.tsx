import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";

import { useAuth } from "@/authContext";

import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableWrap } from "@/components/app-ui/table";
import { useLanguage, useTranslation } from "@/lib/i18n";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { formatFileSize } from "@/utils/format-file-size";
import { useUserIds, userLink } from "@/routes/workspace/repositories/-components/detail/artifacts-tab";

import type { Repository } from "@/services/v1/openapi-types";

// Typing "120" passes through "1" and "12"; waiting for a pause keeps those
// from each firing a scan of the repository.
const PREVIEW_DEBOUNCE_MS = 400;

function useDebounced<T>(value: T, ms: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return settled;
}

function idleFor(iso: string, language: string): string {
  const days = Math.floor((Date.now() - new Date(iso).getTime()) / 86_400_000);
  return new Intl.RelativeTimeFormat(language, { numeric: "always" }).format(-Math.max(days, 0), "day");
}

function ociName(path: string): string {
  return path.slice(0, path.lastIndexOf("/manifests/"));
}

// RetentionPreview lists what the reaper would delete if the days being typed
// were saved, so an administrator sees the impact before pressing Save.
export function RetentionPreview({ repo, days }: { repo: Repository; days: number }) {
  const { t } = useTranslation();
  const language = useLanguage();
  const { me } = useAuth();
  const userIds = useUserIds(Boolean(me.admin || me.auditor));
  const settledDays = useDebounced(days, PREVIEW_DEBOUNCE_MS);
  const isOci = repo.format === "oci";

  const preview = useQuery({
    ...openApiQueryOptions.getRepositoryRetentionPreview({ path: { id: repo.id }, query: { days: settledDays } }),
    enabled: settledDays > 0,
    meta: { suppressGlobalErrorToast: true },
  });

  if (!(days > 0)) return null;

  const error = getErrorMessageIfAny(preview.error);
  const data = preview.data;
  const pending = settledDays !== days || preview.isFetching;

  return (
    <div className="mt-4 rounded-md border border-border bg-muted/20 p-3" aria-live="polite">
      <p className="m-0 text-xs font-medium uppercase text-muted-foreground">{t("repo.retention-preview")}</p>
      {error ? (
        <p className="mb-0 mt-2 text-sm text-destructive">{error}</p>
      ) : !data || (pending && !data) ? (
        <p className="mb-0 mt-2 text-sm text-muted-foreground">{t("repo.retention-preview-loading")}</p>
      ) : data.count === 0 ? (
        <p className="mb-0 mt-2 text-sm text-muted-foreground">{t(isOci ? "repo.retention-preview-none-oci" : "repo.retention-preview-none")}</p>
      ) : (
        <>
          <p className={pending ? "mb-0 mt-2 text-sm text-muted-foreground" : "mb-0 mt-2 text-sm text-destructive"}>
            {isOci
              ? t(data.count === 1 ? "repo.retention-preview-count-oci-one" : "repo.retention-preview-count-oci").replace("{count}", data.count.toLocaleString())
              : t(data.count === 1 ? "repo.retention-preview-count-one" : "repo.retention-preview-count")
                .replace("{count}", data.count.toLocaleString())
                .replace("{size}", formatFileSize(data.size ?? 0))}
          </p>
          <TableWrap className="mt-2">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{isOci ? t("oci.image") : t("common.path")}</TableHead>
                  <TableHead className="w-px pr-4">{isOci ? t("oci.tag") : t("common.version")}</TableHead>
                  {!isOci && <TableHead className="w-px pr-4">{t("common.size")}</TableHead>}
                  <TableHead className="w-px pr-4">{t(isOci ? "repo.retention-preview-last-active-oci" : "repo.retention-preview-last-active")}</TableHead>
                  <TableHead className="w-px">{t(isOci ? "oci.pushed-by" : "repo.retention-preview-uploaded-by")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.items.map((item) => (
                  <TableRow key={item.path}>
                    <TableCell className="max-w-0 truncate font-mono text-xs">
                      {isOci ? (
                        <Link
                          to="/workspace/repositories/$id/oci-artifact"
                          params={{ id: String(repo.id) }}
                          search={{ name: ociName(item.path), ref: item.version }}
                          className="hover:underline"
                        >
                          {ociName(item.path)}
                        </Link>
                      ) : (
                        <Link
                          to="/workspace/repositories/$id/$tab"
                          params={{ id: String(repo.id), tab: "artifacts" }}
                          search={{ q: item.path }}
                          className="hover:underline"
                        >
                          {item.path}
                        </Link>
                      )}
                    </TableCell>
                    <TableCell className="pr-4">{item.version || "-"}</TableCell>
                    {!isOci && <TableCell className="pr-4 text-muted-foreground">{formatFileSize(item.size ?? 0)}</TableCell>}
                    <TableCell className="pr-4 text-muted-foreground">
                      {item.last_active_at.slice(0, 10)} ({idleFor(item.last_active_at, language)})
                    </TableCell>
                    <TableCell>
                      {item.uploaded_by
                        ? userLink(item.uploaded_by, userIds)
                        : <span className="text-muted-foreground">{t("common.anonymous")}</span>}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableWrap>
          {data.count > data.items.length && (
            <p className="mb-0 mt-2 text-xs text-muted-foreground">
              {t("repo.retention-preview-more").replace("{count}", (data.count - data.items.length).toLocaleString())}
            </p>
          )}
        </>
      )}
    </div>
  );
}
