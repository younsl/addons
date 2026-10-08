import type { ReactNode } from "react";

import { useTranslation } from "@/lib/i18n";
import { StatTile } from "@/routes/admin/-storage/components/stat-tile";
import { StorageUsageBar } from "@/routes/admin/-storage/components/storage-usage-bar";
import { formatFileSize } from "@/utils/format-file-size";

import type { ClusterStats } from "@/services/v1/openapi-types";

const count = (n: number | undefined) => (n === undefined ? "-" : n.toLocaleString());

export function ClusterPanel({
  cluster,
  providerName,
  health,
}: {
  cluster: ClusterStats;
  providerName?: string;
  health?: ReactNode;
}) {
  const { t } = useTranslation();
  // A cluster reporting no capacity has nothing to show a proportion of; the
  // ratio would be meaningless rather than zero.
  const usagePct = cluster.total_capacity_bytes > 0 ? cluster.usage_ratio * 100 : null;

  return (
    <section data-testid="panel-cluster">
      <h2 className="mb-3 text-base font-semibold">
        {providerName ? `${providerName} ${t("storage.cluster")}` : t("storage.cluster")}
      </h2>

      {(health || usagePct !== null) && (
        <div className="mb-3 grid gap-3 lg:grid-cols-2">
          {health}
          {usagePct !== null && (
            <StorageUsageBar
              usedBytes={cluster.used_bytes}
              totalBytes={cluster.total_capacity_bytes}
              usagePct={usagePct}
            />
          )}
        </div>
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile label={t("storage.capacity")} value={formatFileSize(cluster.total_capacity_bytes)} />
        <StatTile label={t("storage.available")} value={formatFileSize(cluster.available_bytes)} />
        <StatTile
          label={t("storage.objects")}
          value={count(cluster.object_count)}
          hint={cluster.logical_used_bytes === undefined
            ? undefined
            : `${formatFileSize(cluster.logical_used_bytes)} ${t("storage.logical")}`}
        />
        <StatTile label={t("storage.buckets")} value={count(cluster.bucket_count)} />
        <StatTile
          label={t("storage.drives")}
          value={
            <span className={cluster.offline_drives > 0 ? "text-[var(--fx-severity-critical)]" : undefined}>
              {cluster.online_drives} / {cluster.online_drives + cluster.offline_drives}
            </span>
          }
          hint={cluster.offline_drives > 0
            ? `${cluster.offline_drives} ${t("storage.offline")}`
            : t("storage.all-online")}
        />
        <StatTile label={t("storage.servers")} value={cluster.servers.toLocaleString()} />
        <StatTile
          label={t("storage.version")}
          value={<span className="break-all text-sm font-normal">{cluster.version || "-"}</span>}
          hint={t("storage.version-hint")}
        />
      </div>
    </section>
  );
}
