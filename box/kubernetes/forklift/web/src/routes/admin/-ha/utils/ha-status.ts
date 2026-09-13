import type { StorageStats } from "@/services/v1/openapi-types";

// StorageUsage is how full the store artifacts land on is, normalized across
// the two backends that have a capacity at all.
export type StorageUsage = {
  ratio: number;
  usedBytes: number;
  totalBytes: number;
};

// storageUsage picks the utilization to draw on the topology: the MinIO cluster
// for a MinIO endpoint, the PersistentVolume for the filesystem backend. Plain
// AWS S3 returns null and gets no bar - a bucket has no capacity to run out of,
// so a "94% full" reading there would be inventing a limit that does not exist.
export function storageUsage(stats: StorageStats | undefined | null): StorageUsage | null {
  if (!stats) return null;

  const minio = stats.minio;
  if (minio && minio.total_capacity_bytes > 0) {
    return {
      ratio: minio.usage_ratio,
      usedBytes: minio.used_bytes,
      totalBytes: minio.total_capacity_bytes,
    };
  }

  const fs = stats.fs;
  if (stats.backend === "fs" && fs && fs.total_bytes > 0) {
    return { ratio: fs.usage_ratio, usedBytes: fs.used_bytes, totalBytes: fs.total_bytes };
  }

  return null;
}

// formatUptime renders the elapsed time since startedAt as "Xd Yh Zm Ws",
// dropping leading zero units. Recomputed on each render (the countdown tick)
// so it counts up live.
export function formatUptime(startedAt: string): string {
  const ms = Date.now() - new Date(startedAt).getTime();
  // A clock skew between pod and browser can put the start in the future;
  // "-3s of uptime" is worse than saying nothing.
  if (!isFinite(ms) || ms < 0) return "-";

  const totalSeconds = Math.floor(ms / 1000);
  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  const parts: string[] = [];
  if (days) parts.push(`${days}d`);
  if (hours || days) parts.push(`${hours}h`);
  if (minutes || hours || days) parts.push(`${minutes}m`);
  parts.push(`${seconds}s`);

  return parts.join(" ");
}
