import { TriangleAlert } from "lucide-react";

import { CopyIconButton } from "@/components/app-ui/copy-button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { type MessageKey, useTranslation } from "@/lib/i18n";
import { hasProviderLogo, ProviderLogo } from "@/routes/admin/-storage/components/provider-logo";
import { StatTile } from "@/routes/admin/-storage/components/stat-tile";
import { formatFileSize } from "@/utils/format-file-size";

import type { StorageStats } from "@/services/v1/openapi-types";

const DEPRECATION_MESSAGES: Record<string, MessageKey> = {
  minio: "storage.provider-deprecated-minio",
};

// The backend and forklift's own deduplicated blob footprint. Available for
// every backend, unlike the cluster panel below it.
export function StorageOverview({ storage }: { storage: StorageStats }) {
  const { t } = useTranslation();
  // Artifacts the server has observed to be missing their bytes. Reported here
  // because it spans repositories: the per-repository Artifacts table only
  // marks the rows a user happens to be looking at. A single number is all this
  // page gives - which paths, which response codes, and the repair action
  // belong to the repository that owns them.
  const broken = storage.dangling ?? [];
  const deprecationKey = storage.provider ? DEPRECATION_MESSAGES[storage.provider] : undefined;
  const deprecation = storage.provider_deprecation
    ? deprecationKey ? t(deprecationKey) : storage.provider_deprecation
    : undefined;

  return (
    <section data-testid="panel-overview">
      <h2 className="mb-3 text-base font-semibold">{t("storage.overview")}</h2>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile
          label={t("storage.info")}
          value={
            <div className="flex flex-wrap items-center gap-2">
              {storage.backend === "s3" && hasProviderLogo(storage.provider) && (
                <ProviderLogo id={storage.provider} className="size-7" />
              )}
              <span className="text-base">
                {storage.backend === "s3"
                  ? storage.provider_name || t("storage.mode-s3")
                  : t("storage.mode-filesystem")}
              </span>
              {storage.cluster?.version && (
                <span
                  className="break-all text-base text-muted-foreground"
                  data-testid="storage-provider-version"
                >
                  {storage.cluster.version}
                </span>
              )}
              {deprecation && (
                <Tooltip>
                  <TooltipTrigger
                    render={
                      <span
                        tabIndex={0}
                        className="inline-flex shrink-0 cursor-help"
                        aria-label={`${t("storage.provider-deprecated")}: ${deprecation}`}
                        data-testid="storage-provider-deprecated"
                      />
                    }
                  >
                    <TriangleAlert className="size-5 text-[var(--warning)]" aria-hidden />
                  </TooltipTrigger>
                  <TooltipContent className="max-w-xs">
                    <span className="flex flex-col gap-1">
                      <span className="text-xs font-medium">{t("storage.provider-deprecated")}</span>
                      <span className="text-xs">{deprecation}</span>
                    </span>
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
          }
        />
        {storage.backend === "s3" && (
          <StatTile
            label={t("storage.endpoint")}
            value={
              <span className="flex min-w-0 items-center gap-1">
                <span className="break-all text-sm font-normal">
                  {storage.endpoint || "s3.amazonaws.com"}
                </span>
                <CopyIconButton value={storage.endpoint || "s3.amazonaws.com"} />
              </span>
            }
          />
        )}
        {storage.backend === "s3" && (
          <StatTile
            label={t("storage.bucket")}
            value={<span className="break-all text-sm font-normal">{storage.bucket || "-"}</span>}
            hint={storage.prefix ? `prefix: ${storage.prefix}` : undefined}
          />
        )}
        <StatTile
          label={t("storage.blobs")}
          value={storage.blob_count.toLocaleString()}
          hint={t("storage.blobs-hint")}
        />
        <StatTile
          label={t("storage.stored")}
          value={formatFileSize(storage.blob_bytes)}
          hint={t("storage.stored-hint")}
        />
        {/* Same label as the repository Statistics panel: one condition should
            not have two names depending on which page you opened. */}
        <StatTile
          label={t("repo.stat-broken")}
          value={
            <span
              className={
                broken.length === 0
                  ? "text-[var(--success)]"
                  : "text-[var(--fx-severity-critical)]"
              }
            >
              {broken.length.toLocaleString()}
            </span>
          }
          hint={broken.length === 0
            ? t("storage.consistency-ok-hint")
            : t("storage.consistency-see-statistics")}
        />
      </div>
    </section>
  );
}
