import { Database } from "lucide-react";

import aws from "@/assets/storage-providers/aws.svg";
import garage from "@/assets/storage-providers/garage.svg";
import minio from "@/assets/storage-providers/minio.png";
import rustfs from "@/assets/storage-providers/rustfs.png";
import seaweedfs from "@/assets/storage-providers/seaweedfs.svg";
import { cn } from "@/lib/utils";

import type { MigrationLocation } from "@/services/v1/openapi-types";

// Official marks, bundled so the page never loads a third-party host.
const providers: Record<string, { name: string; logo?: string; plate?: boolean }> = {
  aws: { name: "Amazon S3", logo: aws },
  minio: { name: "MinIO", logo: minio, plate: true },
  rustfs: { name: "RustFS", logo: rustfs, plate: true },
  seaweedfs: { name: "SeaweedFS", logo: seaweedfs },
  garage: { name: "Garage", logo: garage, plate: true },
  generic: { name: "S3-compatible" },
};

export function providerName(id?: string) {
  return (id && providers[id]?.name) || "S3";
}

export function ProviderLogo({ id, className }: { id?: string; className?: string }) {
  const p = id ? providers[id] : undefined;
  if (!p?.logo) {
    return (
      <span className={cn("grid size-6 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground", className)}>
        <Database className="size-3.5" />
      </span>
    );
  }
  return (
    <img
      src={p.logo}
      alt={p.name}
      className={cn("size-6 shrink-0 rounded-md object-contain", p.plate && "bg-white p-0.5", className)}
    />
  );
}

export function hostOf(endpoint: string) {
  if (!endpoint) return "s3.amazonaws.com";
  try {
    return new URL(endpoint).host;
  } catch {
    return endpoint;
  }
}

export function Location({ location, className }: { location: MigrationLocation; className?: string }) {
  const where = [hostOf(location.endpoint), location.bucket, location.prefix].filter(Boolean).join(" / ");
  return (
    <div className={cn("flex min-w-0 items-center gap-2", className)}>
      <ProviderLogo id={location.provider} />
      <div className="min-w-0">
        <div className="truncate text-[13px] font-medium">{providerName(location.provider)}</div>
        <div className="truncate text-[11px] tabular-nums text-muted-foreground" title={where}>
          {where}
        </div>
      </div>
    </div>
  );
}
