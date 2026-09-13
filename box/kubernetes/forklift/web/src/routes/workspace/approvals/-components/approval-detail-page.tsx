import { useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { ApprovalStatusBadge } from "@/components/app-ui/status-badge";
import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyIconButton } from "@/components/app-ui/copy-button";
import { PageHeader } from "@/components/app-ui/page";
import { useAuth } from "@/authContext";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { ApprovalNotificationPanel } from "@/routes/workspace/approvals/-components/approval-notification-panel";
import { ApprovalReviewersPanel } from "@/routes/workspace/approvals/-components/approval-reviewers-panel";
import { OvsAnalysis } from "@/routes/workspace/approvals/-components/ovs-analysis";
import { ReviewModal } from "@/routes/workspace/approvals/-components/review-modal";
import { useApprovalDetail } from "@/routes/workspace/approvals/-hooks/use-approval-detail";
import { canReviewApprovals } from "@/utils/permissions";

// ApprovalDetailPage is the per-request review screen: it shows the full
// approval metadata and the OSV vulnerability analysis so a reviewer can judge
// the package before deciding. The decision itself is made in the shared
// ReviewModal.
export function ApprovalDetailPage() {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const { me } = useAuth();
  const navigate = useNavigate();
  const { id } = useParams({ strict: false }) as { id?: string };
  const { approval, error, isLoading } = useApprovalDetail(Number(id));
  const [isReviewing, setIsReviewing] = useState(false);
  // Auditors have read-only access: they can open this page but not decide.
  const canDecide = canReviewApprovals(me);

  if (isLoading) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;
  if (!approval) return <Alert className="my-2.5">{error || "Approval not found."}</Alert>;

  // The package-specific URL when the scan resolved one, else the repository's
  // upstream. Only rendered inside the upstream_url guard below, so it is a
  // string wherever it is used.
  const upstreamUrl = approval.upstream_package_url || approval.upstream_url || "";

  return (
    <div data-testid="page-approval-detail">
      <PageHeader
        title={
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <span className="font-mono">{approval.package}</span>
            <ApprovalStatusBadge status={approval.status} />
            {approval.notified_receivers && approval.notified_receivers.length > 0 && (
              <Badge variant="secondary">{t("approval.noted")}</Badge>
            )}
          </div>
        }
        actions={
          <>
            {canDecide && (
              <Button onClick={() => setIsReviewing(true)}>{t("common.review")}</Button>
            )}
            <Button variant="outline" onClick={() => navigate({ to: "/workspace/approvals" })}>
              {t("approval.back")}
            </Button>
          </>
        }
      />
      {error && <Alert className="mb-4">{error}</Alert>}

      <Card size="sm" className="mb-4" data-testid="panel-request">
        <CardContent>
          <h2 className="m-0 mb-4 text-base font-semibold">{t("common.request")}</h2>
          <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-5 gap-y-2 [&_dd]:m-0 [&_dt]:text-muted-foreground">
            <dt>{t("common.repository")}</dt><dd>{approval.repo_name}</dd>
            <dt>{t("common.package")}</dt><dd className="font-mono">{approval.package}</dd>
            <dt>{t("approval.requested-version")}</dt>
            <dd className="font-mono">
              {approval.last_requested_version || (
                <span className="text-muted-foreground">{t("approval.version-unknown")}</span>
              )}
            </dd>
            {approval.upstream_url && (
              <>
                <dt>{t("common.upstream")}</dt>
                <dd className="min-w-0">
                  <span className="inline-flex min-w-0 items-center gap-1.5">
                    <a
                      className="break-all font-mono text-xs underline underline-offset-4 hover:no-underline"
                      href={upstreamUrl}
                      target="_blank"
                      rel="noreferrer"
                    >
                      {upstreamUrl}
                    </a>
                    <CopyIconButton value={upstreamUrl} />
                  </span>
                </dd>
              </>
            )}
            <dt>{t("approval.requested-by")}</dt>
            <dd>
              {approval.requested_by || (
                <span className="text-muted-foreground">{t("common.anonymous")}</span>
              )}
            </dd>
            <dt>{t("common.requests")}</dt><dd>{approval.request_count}</dd>
            <dt>{t("approval.first-requested")}</dt>
            <dd className="text-muted-foreground">{fmtDate(approval.first_requested_at)}</dd>
            <dt>{t("approval.last-requested")}</dt>
            <dd className="text-muted-foreground">{fmtDate(approval.last_requested_at)}</dd>
            {approval.decided_by && (
              <><dt>{t("approval.decided-by")}</dt><dd>{approval.decided_by}</dd></>
            )}
            {approval.decided_at && (
              <>
                <dt>{t("approval.decided-at")}</dt>
                <dd className="text-muted-foreground">{fmtDate(approval.decided_at)}</dd>
              </>
            )}
            {approval.note && <><dt>{t("common.note")}</dt><dd>{approval.note}</dd></>}
          </dl>
        </CardContent>
      </Card>

      <OvsAnalysis approval={approval} />
      <ApprovalNotificationPanel approval={approval} />
      <ApprovalReviewersPanel reviewers={approval.reviewers} />

      {isReviewing && (
        <ReviewModal
          row={approval}
          // The decision invalidates this page's own query, so closing is all
          // that is left to do.
          onDone={() => setIsReviewing(false)}
          onCancel={() => setIsReviewing(false)}
        />
      )}
    </div>
  );
}
