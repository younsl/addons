import { Badge } from "@/components/app-ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useTranslation } from "@/lib/i18n";

// ApprovalReviewersPanel lists the users permitted to approve this repository,
// so it is clear who can act on the request. OIDC-group approvers who have
// never signed in are not enumerable and so are not shown - the list is who we
// know can act, not necessarily everyone who can.
export function ApprovalReviewersPanel({ reviewers }: { reviewers?: string[] }) {
  const { t } = useTranslation();

  return (
    <Card size="sm" className="mb-4" data-testid="panel-reviewers">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">
          {t("common.reviewers")}{" "}
          <span className="text-xs font-normal text-muted-foreground">
            {t("repo.approvers-subtitle")}
          </span>
        </h2>
        {!reviewers || reviewers.length === 0 ? (
          <p className="mb-0 text-sm text-muted-foreground">{t("approval.no-approvers")}</p>
        ) : (
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            {reviewers.map((reviewer) => <Badge key={reviewer}>{reviewer}</Badge>)}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
