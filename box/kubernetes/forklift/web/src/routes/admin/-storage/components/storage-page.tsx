import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Switch } from "@/components/ui/switch";
import { useTranslation } from "@/lib/i18n";
import { MinioClusterPanel } from "@/routes/admin/-storage/components/minio-cluster-panel";
import { StorageOverview } from "@/routes/admin/-storage/components/storage-overview";
import { useStorageStatus } from "@/routes/admin/-storage/hooks/use-storage-status";
import { formatTimestamp } from "@/utils/format-timestamp";

// StoragePage is the object-storage operations overview: the active backend and
// forklift's deduplicated blob footprint (always available), plus - for a MinIO
// backend - live cluster capacity, usage, object and bucket counts, and drive
// health from the MinIO Admin API.
export function StoragePage() {
  const { t } = useTranslation();
  const { error, isAutoRefreshing, isLoading, refresh, setAutoRefreshing, storage, updatedAt } =
    useStorageStatus();

  return (
    <div data-testid="page-storage">
      <PageHeader
        title={t("storage.title")}
        actions={
          <div className="flex min-w-0 items-center gap-3 max-sm:flex-wrap">
            <label className="flex items-center gap-2 text-sm text-muted-foreground">
              <Switch
                checked={isAutoRefreshing}
                onCheckedChange={(checked) => setAutoRefreshing(checked === true)}
              />
              {t("storage.auto-refresh")}
            </label>
            <Button variant="outline" onClick={refresh}>{t("common.refresh")}</Button>
          </div>
        }
      />
      <PageDescription>{t("storage.description")}</PageDescription>

      {updatedAt && (
        <p className="mb-4 text-xs text-muted-foreground">
          {t("storage.last-updated")}:{" "}
          <span className="tabular-nums text-foreground">{formatTimestamp(updatedAt)}</span>
        </p>
      )}
      {error && <Alert className="mb-4">{error}</Alert>}
      {isLoading || !storage ? (
        <div className="text-sm text-muted-foreground">{t("common.loading")}</div>
      ) : (
        <div className="space-y-6">
          <StorageOverview storage={storage} />

          {storage.minio ? (
            <MinioClusterPanel minio={storage.minio} />
          ) : storage.minio_error ? (
            // The backend claims MinIO but the admin API did not answer. Worth
            // saying plainly: the cluster panel being absent is a symptom, not
            // a configuration choice.
            <Alert>{t("storage.minio-unavailable")}: {storage.minio_error}</Alert>
          ) : storage.backend !== "s3" ? (
            <div className="rounded-md border border-dashed border-[var(--fx-border-subtle)] px-3 py-6 text-center text-sm text-muted-foreground">
              {t("storage.fs-note")}
            </div>
          ) : null}
        </div>
      )}
    </div>
  );
}
