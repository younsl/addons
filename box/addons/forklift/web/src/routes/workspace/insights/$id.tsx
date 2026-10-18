import { createFileRoute, Link } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";
import type { ReactNode } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { CopyLinkButton } from "@/components/app-ui/copy-button";
import { PageHeader } from "@/components/app-ui/page";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableWrap } from "@/components/app-ui/table";
import { Card, CardContent } from "@/components/ui/card";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { formatAbsolute, formatRelative } from "@/utils/format-relative";
import { AffectedLink, InsightSeverityBadge, StatusLabel, useInsightText } from "./-components/finding-card";
import { AckButton, AckPanel, canAcknowledge } from "./-components/ack-panel";
import { InsightsTabs } from "./-components/insights-tabs";
import { CheckHistory } from "./-components/check-history";

// One check on its own page, so it can be linked to: what it looks at, why it
// matters, when it fires, what to do, and everything it flagged. The layout
// follows a Trusted Advisor check: category and status up top, then the
// description, the alert criteria, the recommended action and the flagged items.
// `from` and `to` hold a zoom into the Metrics charts, so a zoomed view is a
// link like any other.
type DetailSearch = { from?: string; to?: string };

export const Route = createFileRoute("/workspace/insights/$id")({
  validateSearch: (search: Record<string, unknown>): DetailSearch => {
    const out: DetailSearch = {};
    if (typeof search.from === "string" && !Number.isNaN(Date.parse(search.from))) out.from = search.from;
    if (typeof search.to === "string" && !Number.isNaN(Date.parse(search.to))) out.to = search.to;
    return out;
  },
  component: InsightDetailRoute,
});

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-5">
      <h2 className="m-0 mb-1.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">{title}</h2>
      <div className="text-sm leading-relaxed">{children}</div>
    </section>
  );
}

function InsightDetailRoute() {
  const { id } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const { t } = useTranslation();
  const text = useInsightText();
  const query = useQuery({ ...openApiQueryOptions.getInsights(), meta: { suppressGlobalErrorToast: true } });
  const report = query.data?.report ?? null;
  // The page answers to the check id or its code, so /insights/AC-05 works as
  // well as /insights/tokens-unscoped-admin.
  const check = report?.checks.find((c) => c.id === id || (c.code ?? "").toLowerCase() === id.toLowerCase());
  const finding = check ? report?.findings.find((f) => f.id === check.id) : undefined;
  const error = getErrorMessageIfAny(query.error);

  const back = (
    <Link to="/workspace/insights" className="mb-3 inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
      <ArrowLeft className="size-4" aria-hidden="true" /> {t("insights.back")}
    </Link>
  );

  if (error) return <>{back}<Alert>{error}</Alert></>;
  if (!query.data) return <>{back}<p className="text-sm text-muted-foreground">{t("common.loading")}</p></>;
  // Check pages are for administrators, auditors and security reviewers. A
  // viewer limited to some repositories reaches the list only, even by link.
  if (query.data.scope !== "all") {
    return <>{back}<Card><CardContent className="text-sm text-muted-foreground">{t("insights.check-restricted")}</CardContent></Card></>;
  }
  if (!report || !check) {
    return <>{back}<Card><CardContent className="text-sm text-muted-foreground">{t("insights.check-missing")}</CardContent></Card></>;
  }

  const hidden = finding ? finding.total - finding.affected.length : 0;
  return (
    <>
      <InsightsTabs active="security" />
      {back}
      <PageHeader
        title={text.title(check)}
        actions={
          <>
            <CopyLinkButton />
            {canAcknowledge(check, Boolean(query.data?.can_refresh)) && <AckButton check={check} />}
          </>
        }
      />

      <Card className="mb-5">
        {/* Fields take their own width rather than equal columns, so a long status
            such as "Investigation recommended" stays on one line; on a narrow
            screen whole fields wrap instead. */}
        <CardContent className="flex flex-wrap gap-x-12 gap-y-4">
          <div>
            <div className="text-xs text-muted-foreground">{t("insights.code")}</div>
            <div className="mt-1 text-sm">{check.code || "-"}</div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">{t("insights.status")}</div>
            <div className="mt-1"><StatusLabel status={check.status} /></div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">{t("insights.severity")}</div>
            <div className="mt-1">{finding ? <InsightSeverityBadge severity={finding.severity} /> : <span className="text-sm text-muted-foreground">-</span>}</div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">{t("insights.category")}</div>
            <div className="mt-1 text-sm">
              <Link to="/workspace/insights" search={{ category: check.category }}>
                {t(`insights.category-${check.category}` as MessageKey)}
              </Link>
            </div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">{t("insights.audience")}</div>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {check.audiences.map((a) => <Badge key={a} variant="secondary">{t(`insights.audience-${a}` as MessageKey)}</Badge>)}
            </div>
          </div>
        </CardContent>
      </Card>

      {finding && (
        <Section title={t("insights.result")}>{text.summary(finding)}</Section>
      )}
      {!finding && check.status === "ok" && (
        <Section title={t("insights.result")}>{t("insights.result-ok")}</Section>
      )}
      {check.status === "unavailable" && (
        <Section title={t("insights.result")}>{t("insights.result-unavailable")}</Section>
      )}
      {query.data?.scope === "all" && (
        <CheckHistory checkId={check.id} range={search}
          onRange={(range) => navigate({ search: range, replace: true, resetScroll: false })} />
      )}
      <AckPanel check={check} canAck={Boolean(query.data?.can_refresh)} />
      <Section title={t("insights.why")}>{text.why(check)}</Section>
      <Section title={t("insights.criteria")}>{text.criteria(check)}</Section>
      <Section title={t("insights.recommendation")}>{text.action(check)}</Section>

      {finding && (
        <Section title={`${t("insights.flagged-items")} (${finding.total.toLocaleString()})`}>
          <TableWrap>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-px pr-4">{t("insights.resource-type")}</TableHead>
                  <TableHead>{t("insights.affected")}</TableHead>
                  <TableHead className="w-px pr-4">{t("common.repository")}</TableHead>
                  <TableHead>{t("insights.detail")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {finding.affected.map((item, i) => (
                  <TableRow key={`${item.kind}:${item.label}:${item.repo ?? ""}:${i}`}>
                    <TableCell className="whitespace-nowrap pr-4 text-muted-foreground">{t(`insights.kind-${item.kind}` as MessageKey)}</TableCell>
                    <TableCell className="max-w-0 truncate font-mono text-xs"><AffectedLink finding={finding} item={item} /></TableCell>
                    <TableCell className="pr-4 text-muted-foreground">{item.repo || "-"}</TableCell>
                    <TableCell className="text-muted-foreground">{item.detail || "-"}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableWrap>
          {hidden > 0 && (
            <p className="mb-0 mt-2 text-xs text-muted-foreground">{t("insights.more").replace("{count}", hidden.toLocaleString())}</p>
          )}
        </Section>
      )}

      <p className="text-xs text-muted-foreground">
        {t("insights.analyzed")} {formatRelative(report.generated_at)} ({formatAbsolute(report.generated_at)})
      </p>
    </>
  );
}
