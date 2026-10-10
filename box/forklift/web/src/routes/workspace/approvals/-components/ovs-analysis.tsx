import { Card, CardContent } from "@/components/ui/card";
import { SeverityBar } from "@/components/app-ui/severity-bar";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { AdvisoryTable } from "@/routes/workspace/approvals/-components/advisory-table";
import { formatMilliseconds } from "@/utils/format-duration";

import type { PackageApproval } from "@/services/v1/openapi-types";

// OvsAnalysis renders the OSV scan result: a large severity bar, the scan
// metadata (result, scope, when, how long), and a table of advisories with id,
// severity, CVSS score and a link to osv.dev. Empty until the async scan lands.
export function OvsAnalysis({ approval }: { approval: PackageApproval }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const advisories = approval.vuln_advisories ?? [];
  const ids = approval.vuln_ids ?? [];
  const isPackageScope = approval.vuln_scope === "package";
  const isClean = approval.vuln_severity === "none";

  return (
    <Card size="sm" className="mb-4" data-testid="panel-vuln-analysis">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("approval.vuln-analysis")}</h2>
        {approval.vuln_severity === undefined ? (
          <p className="m-0 text-sm leading-relaxed text-muted-foreground">
            {t("approval.not-scanned")}
          </p>
        ) : (
          <>
            <div className="my-4">
              <SeverityBar
                severity={approval.vuln_severity}
                counts={approval.vuln_counts}
                scope={approval.vuln_scope}
                source={approval.vuln_source}
                scannedAt={approval.vuln_scanned_at}
                advisories={approval.vuln_advisories}
                size="lg"
              />
            </div>
            <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-5 gap-y-2 [&_dd]:m-0 [&_dt]:text-muted-foreground">
              <dt>{t("approval.data-source")}</dt>
              <dd>
                {!approval.vuln_source || approval.vuln_source === "OSV" ? (
                  <a
                    className="underline underline-offset-4 hover:no-underline"
                    href="https://osv.dev"
                    target="_blank"
                    rel="noreferrer"
                  >
                    {t("approval.osv-source")}
                  </a>
                ) : (
                  approval.vuln_source
                )}
              </dd>
              <dt>{t("common.result")}</dt>
              <dd>
                {isClean
                  ? t("approval.clean")
                  : <>{t("approval.vulnerable-highest")} <strong>{approval.vuln_severity}</strong></>}
              </dd>
              <dt>{t("common.scope")}</dt>
              <dd>
                {isPackageScope
                  ? t("approval.package-level")
                  : `Version ${approval.last_requested_version}`}
              </dd>
              <dt>{t("approval.scanned-at")}</dt>
              <dd className="text-muted-foreground">
                {approval.vuln_scanned_at ? fmtDate(approval.vuln_scanned_at) : t("common.na")}
              </dd>
              <dt>{t("common.duration")}</dt>
              <dd className="text-muted-foreground">
                {approval.vuln_scan_ms != null
                  ? formatMilliseconds(approval.vuln_scan_ms)
                  : t("common.na")}
              </dd>
            </dl>
            {/* Advisory detail is the best case; an older scan may have only the
                ids, and a clean one has neither. */}
            {advisories.length > 0 ? (
              <AdvisoryTable advisories={advisories} />
            ) : ids.length > 0 ? (
              <ul className="mb-0 mt-2.5 columns-2 pl-[18px] [column-gap:28px] max-[760px]:columns-1 [&_li]:break-inside-avoid [&_li]:font-mono [&_li]:text-[13px]">
                {ids.map((id) => (
                  <li key={id}>
                    <a
                      className="underline underline-offset-4 hover:no-underline"
                      href={`https://osv.dev/${id}`}
                      target="_blank"
                      rel="noreferrer"
                    >
                      {id}
                    </a>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="mb-0">{t("approval.no-advisories")}</p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}
