import { useTranslation } from "@/lib/i18n";
import { StatTile } from "@/routes/admin/-storage/components/stat-tile";
import { StorageUsageBar } from "@/routes/admin/-storage/components/storage-usage-bar";
import { formatFileSize } from "@/utils/format-file-size";

import type { MinioStats } from "@/services/v1/openapi-types";

// Live cluster capacity, usage, object and bucket counts, and drive health,
// read from the MinIO Admin API. Only present when the backend is MinIO.
export function MinioClusterPanel({ minio }: { minio: MinioStats }) {
  const { t } = useTranslation();
  // A cluster reporting no capacity has nothing to show a proportion of; the
  // ratio would be meaningless rather than zero.
  const usagePct = minio.total_capacity_bytes > 0 ? minio.usage_ratio * 100 : null;

  return (
    <section data-testid="panel-minio-cluster">
      <h2 className="mb-3 text-base font-semibold">{t("storage.minio-cluster")}</h2>

      {usagePct !== null && (
        <StorageUsageBar
          usedBytes={minio.used_bytes}
          totalBytes={minio.total_capacity_bytes}
          usagePct={usagePct}
        />
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile label={t("storage.capacity")} value={formatFileSize(minio.total_capacity_bytes)} />
        <StatTile label={t("storage.available")} value={formatFileSize(minio.available_bytes)} />
        <StatTile
          label={t("storage.objects")}
          value={minio.object_count.toLocaleString()}
          hint={`${formatFileSize(minio.logical_used_bytes)} ${t("storage.logical")}`}
        />
        <StatTile label={t("storage.buckets")} value={minio.bucket_count.toLocaleString()} />
        <StatTile
          label={t("storage.drives")}
          value={
            <span className={minio.offline_drives > 0 ? "text-[var(--fx-severity-critical)]" : undefined}>
              {minio.online_drives} / {minio.online_drives + minio.offline_drives}
            </span>
          }
          hint={minio.offline_drives > 0
            ? `${minio.offline_drives} ${t("storage.offline")}`
            : t("storage.all-online")}
        />
        <StatTile label={t("storage.servers")} value={minio.servers.toLocaleString()} />
        <StatTile
          label={t("storage.version")}
          value={<span className="break-all text-sm font-normal">{minio.version || "-"}</span>}
          hint={t("storage.version-hint")}
        />
      </div>
    </section>
  );
}
