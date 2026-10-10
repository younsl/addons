import { Clock, ShieldCheck } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatFileSize } from "@/utils/format-file-size";

// These read the aggregate counts, which only the list endpoint computes.
// RepositoryListItem is the document's name for that wider shape; a plain
// Repository does not carry them.
import type { RepositoryListItem } from "@/services/v1/openapi-types";

// ArtifactCount shows the number of stored artifacts in a boxed count; empty
// repositories (and groups, which store nothing themselves) render a muted 0.
// When a proxy has packages awaiting approval, an extra yellow dashed box flags
// the pending count so the repository stands out to an approver.
export function ArtifactCount({ repo }: { repo: RepositoryListItem }) {
  const count = repo.artifact_count ?? 0;
  const pending = repo.pending_approval_count ?? 0;
  const tip = `${pending.toLocaleString()} package${pending === 1 ? "" : "s"} awaiting approval`;

  return (
    <span className="inline-flex items-center gap-1 whitespace-nowrap">
      <Badge variant={count === 0 ? "outline" : "default"}>{count.toLocaleString()}</Badge>
      {pending > 0 && (
        <Badge variant="warning" className="border-dashed" title={tip}>
          {pending.toLocaleString()}
        </Badge>
      )}
    </span>
  );
}

// RepoSize shows stored bytes, human-readable; proxies with a cache size cap
// also show usage against the cap. Empty repositories render a muted 0 B.
export function RepoSize({ repo }: { repo: RepositoryListItem }) {
  const size = repo.total_size ?? 0;
  const max = repo.config.cache.max_size_bytes;

  return (
    <span className={size === 0 ? "text-muted-foreground" : undefined}>
      {formatFileSize(size)}
      {repo.type === "proxy" && max > 0 && (
        <span className="text-muted-foreground"> / {formatFileSize(max)}</span>
      )}
    </span>
  );
}

// CleanRatio shows the share of scanned artifacts that are clean (no
// advisories) as a percentage: green at 100%, amber when some are vulnerable,
// red when most are. A muted dash means nothing scanned yet (or an unscannable
// format like raw), so the denominator is zero - which is not the same as 0%.
export function CleanRatio({ repo }: { repo: RepositoryListItem }) {
  const scanned = repo.scanned_count ?? 0;
  const clean = repo.clean_count ?? 0;

  if (scanned === 0) return <span className="text-muted-foreground">-</span>;

  const pct = Math.round((clean / scanned) * 100);
  // Status tokens, which hold 4.5:1 on every surface in both themes.
  const tone =
    pct === 100
      ? "text-[var(--fx-success)]"
      : pct >= 50
        ? "text-[var(--fx-warning)]"
        : "text-destructive";

  return (
    <span
      className={cn("whitespace-nowrap tabular-nums", tone)}
      title={`${clean.toLocaleString()} / ${scanned.toLocaleString()} clean`}
    >
      {pct}%
    </span>
  );
}

// SecurityIcons renders approval for proxy and hosted repositories, and the
// upstream release-age control for proxies.
export function SecurityIcons({ repo }: { repo: RepositoryListItem }) {
  const { t } = useTranslation();

  if (repo.type !== "proxy" && repo.type !== "hosted") {
    return <span className="text-muted-foreground">-</span>;
  }

  const age = repo.config.age_policy;
  const approval = repo.config.approval ?? { enabled: false, mode: "enforce" };
  const minAge = age.min_age || "0";
  const ageTip = !age.enabled
    ? t("repo.age-disabled")
    : age.action === "warn"
      ? `Age policy warns about versions published less than ${minAge} ago.`
      : `Age policy blocks versions published less than ${minAge} ago.`;
  const approvalTip = !approval.enabled
    ? t("repo.approval-disabled")
    : approval.mode === "audit"
      ? t("repo.approval-audit")
      : t("repo.approval-on");

  return (
    <span className="inline-flex items-center gap-2">
      {repo.type === "proxy" && (
        <Tooltip>
          <TooltipTrigger render={<span tabIndex={0} aria-label={t("repo.age-policy")} />}>
            <span className={cn("inline-flex text-muted-foreground", age.enabled && "text-accent-ink")}>
              <Clock className="size-4" aria-hidden="true" />
            </span>
          </TooltipTrigger>
          <TooltipContent>{ageTip}</TooltipContent>
        </Tooltip>
      )}
      <Tooltip>
        <TooltipTrigger render={<span tabIndex={0} aria-label={t("repo.package-approval")} />}>
          <span
            className={cn("inline-flex text-muted-foreground", approval.enabled && "text-accent-ink")}
          >
            <ShieldCheck className="size-4" aria-hidden="true" />
          </span>
        </TooltipTrigger>
        <TooltipContent>{approvalTip}</TooltipContent>
      </Tooltip>
    </span>
  );
}
