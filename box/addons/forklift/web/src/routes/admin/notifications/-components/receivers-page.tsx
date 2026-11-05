import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { ReceiversTable } from "@/routes/admin/notifications/-components/receivers-table";
import { useReceiversList } from "@/routes/admin/notifications/-hooks/use-receiver-mutations";

// ReceiversPage lists notification receivers - named alarm channels (webhooks)
// that repositories can select to be alerted when a package is quarantined
// pending approval. Add and edit open a separate page.
export function ReceiversPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const receiversQuery = useReceiversList();
  const receivers = receiversQuery.data;
  const error = getErrorMessageIfAny(receiversQuery.error);

  return (
    <div data-testid="page-notifications">
      <PageHeader title={t("notification.title")} />
      <PageDescription>{t("notification.description")}</PageDescription>

      <div className="mb-4 flex min-w-0 items-center justify-between gap-3 max-sm:flex-col max-sm:items-start">
        <h2 className="m-0 flex items-baseline gap-2 text-base font-semibold">
          {t("notification.receivers")}
          <span className="text-sm font-normal text-muted-foreground">
            {receivers?.length ?? 0}
          </span>
          <span className="text-xs font-normal text-muted-foreground">
            {t("notification.subtitle")}
          </span>
        </h2>
        <Button onClick={() => navigate({ to: "/admin/notifications/new" })}>
          {t("notification.add")}
        </Button>
      </div>
      {error && <Alert className="mb-4">{error}</Alert>}
      {receiversQuery.isPending ? (
        <div className="text-sm text-muted-foreground">{t("common.loading")}</div>
      ) : (
        <ReceiversTable receivers={receivers ?? []} />
      )}
      <p className="mt-4 text-sm text-muted-foreground">{t("notification.webhook-note")}</p>
    </div>
  );
}
