import type { FormEvent } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NAME_PATTERN } from "@/lib/name-pattern";
import { useTranslation } from "@/lib/i18n";
import { TokenCreatedPanel } from "@/routes/workspace/tokens/-components/token-created-panel";
import { TokenExpiryField } from "@/routes/workspace/tokens/-components/token-expiry-field";
import { TokenScopeBuilder } from "@/routes/workspace/tokens/-components/token-scope-builder";
import { useTokenCreateForm } from "@/routes/workspace/tokens/-hooks/use-token-create-form";

// Token creation. Reached from the New token button on /workspace/tokens
// (self-service for the current user) or from a user's detail page at
// /access/users/$id/tokens/new (an admin issuing for that user). The presence
// of the :id route param selects the target and where Done and Cancel return
// to. All fields are required; expiry is capped at one year by the API.
export function TokenNewPage() {
  const { t } = useTranslation();
  const form = useTokenCreateForm();

  const onSubmit = (event: FormEvent) => {
    event.preventDefault();
    form.submit();
  };

  if (form.createdToken) {
    return <TokenCreatedPanel token={form.createdToken} onDone={form.returnTo} />;
  }

  return (
    <div data-testid="page-token-new">
      <PageHeader
        title={form.forUserId !== null ? t("token.create-for-user") : t("token.create")}
      />
      <PageDescription>{t("token.new-description")}</PageDescription>

      <Card size="sm" className="mb-4 max-w-[44rem]">
        <CardContent>
          <form onSubmit={onSubmit} className="space-y-5">
            <FieldGroup className="gap-4">
              <Field>
                <FieldLabel htmlFor="token-name">
                  {t("token.name")}<span className="text-destructive">*</span>
                </FieldLabel>
                <Input
                  id="token-name"
                  value={form.name}
                  onChange={(event) => form.setName(event.target.value)}
                  placeholder="ci"
                  autoFocus
                  required
                  pattern={NAME_PATTERN}
                  title={t("common.name-rule-64")}
                />
                <FieldDescription>{t("common.name-rule")}</FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="token-description">
                  {t("token.description")}<span className="text-destructive">*</span>
                </FieldLabel>
                <Input
                  id="token-description"
                  value={form.description}
                  onChange={(event) => form.setDescription(event.target.value)}
                  placeholder={t("token.description-placeholder")}
                  required
                />
              </Field>

              <TokenExpiryField
                value={form.expiresOn}
                minDate={form.minDate}
                maxDate={form.maxDate}
                onChange={form.setExpiresOn}
              />
            </FieldGroup>

            <TokenScopeBuilder scopes={form.scopes} onChange={form.setScopes} />

            {form.error && <Alert>{form.error}</Alert>}
            <div className="flex min-w-0 items-center gap-2 border-t border-border pt-4 max-sm:flex-wrap">
              <Button type="submit" disabled={!form.isComplete || form.isPending}>
                {t("token.create")}
              </Button>
              <Button variant="outline" type="button" onClick={form.returnTo}>
                {t("common.cancel")}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
