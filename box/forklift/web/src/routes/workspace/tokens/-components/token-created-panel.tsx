import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/app-ui/copy-button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { useTranslation } from "@/lib/i18n";

// The secret, shown once. There is no endpoint that returns it again, which is
// why the page changes into this rather than navigating away on success.
export function TokenCreatedPanel({
  token,
  onDone,
}: {
  token: string;
  onDone: () => void;
}) {
  const { t } = useTranslation();

  return (
    <div data-testid="page-token-created">
      <PageHeader title={t("token.created")} />
      <PageDescription>{t("token.copy-warning")}</PageDescription>
      <Card size="sm" className="mb-4 max-w-[40rem]">
        <CardContent>
          <div className="flex min-w-0 items-stretch gap-2 max-sm:flex-col max-sm:flex-wrap">
            <div className="min-h-8 flex-1 overflow-x-auto rounded-lg border border-border bg-muted px-3 py-2 font-mono text-xs">
              {token}
            </div>
            <CopyButton value={token} />
          </div>
          <Button className="mt-5" onClick={onDone}>
            {t("common.done")}
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
