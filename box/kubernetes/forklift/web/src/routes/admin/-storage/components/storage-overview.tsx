import { Badge } from "@/components/app-ui/badge";
import { useTranslation } from "@/lib/i18n";
import { StatTile } from "@/routes/admin/-storage/components/stat-tile";
import { formatFileSize } from "@/utils/format-file-size";

import type { StorageStats } from "@/services/v1/openapi-types";

// The backend and forklift's own deduplicated blob footprint. Available for
// every backend, unlike the MinIO cluster panel below it.
export function StorageOverview({ storage }: { storage: StorageStats }) {
  const { t } = useTranslation();
  // Artifacts the server has observed to be missing their bytes. Reported here
  // because it spans repositories: the per-repository Artifacts table only
  // marks the rows a user happens to be looking at. A single number is all this
  // page gives - which paths, which response codes, and the repair action
  // belong to the repository that owns them.
  const broken = storage.dangling ?? [];

  return (
    <section data-testid="panel-overview">
      <h2 className="mb-3 text-base font-semibold">{t("storage.overview")}</h2>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile
          label={t("storage.mode")}
          value={
            <Badge>
              {storage.mode === "minio"
                ? t("storage.mode-minio")
                : storage.mode === "s3"
                  ? t("storage.mode-s3")
                  : t("storage.mode-filesystem")}
            </Badge>
          }
          hint={storage.endpoint}
        />
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
