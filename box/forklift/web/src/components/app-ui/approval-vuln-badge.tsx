import { SeverityBadge } from "@/components/app-ui/severity-badge";
import { useTranslation } from "@/lib/i18n";

// ApprovalVulnBadge shows the OSV scan result so a reviewer sees known
// advisories before approving. A scanned coordinate with no advisories shows a
// green "clean" badge; an unscanned one shows muted "not scanned" - the
// distinction matters, since unscanned is not the same as safe.
//
// scope marks whether the scan is for the exact requested version ("version")
// or the package across all versions ("package", shown with a "pkg" suffix),
// the latter used when the requested version is unknown.
export function ApprovalVulnBadge({
  severity,
  ids,
  scope,
}: {
  severity?: string;
  ids?: string[];
  scope?: string;
}) {
  const { t } = useTranslation();

  if (severity === undefined) {
    return <span className="text-muted-foreground">{t("approval.not-scanned-short")}</span>;
  }

  const isPackageScope = scope === "package";
  const suffix = isPackageScope ? " · pkg" : "";
  const scopeTitle = isPackageScope
    ? "package-level scan (requested version unknown)"
    : "scan for the requested version";

  if (severity === "none") {
    return (
      <SeverityBadge severity="none" title={scopeTitle}>
        {t("approval.clean-short")}{suffix}
      </SeverityBadge>
    );
  }

  const count = ids?.length ?? 0;

  return (
    <SeverityBadge
      severity={severity}
      title={`${count ? ids!.join(", ") : severity} · ${scopeTitle}`}
    >
      {severity}{count > 1 ? ` ×${count}` : ""}{suffix}
    </SeverityBadge>
  );
}
