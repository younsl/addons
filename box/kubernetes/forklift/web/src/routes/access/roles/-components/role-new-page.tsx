import { useState, type FormEvent } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NAME_PATTERN } from "@/lib/name-pattern";
import { useTranslation } from "@/lib/i18n";
import { PermissionBuilder } from "@/routes/access/roles/-components/permission-builder";
import { useRoleCreateSubmit } from "@/routes/access/roles/-hooks/use-role-create-submit";

import type { PermissionCreate } from "@/services/v1/openapi-types";

// Admin-only role creation, reached from the Create button on /access/roles.
// Permissions can be granted here at creation, or added later on the role's
// detail page.
export function RoleNewPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [permissions, setPermissions] = useState<PermissionCreate[]>([]);
  const { error, isPending, submit } = useRoleCreateSubmit();

  const onSubmit = (event: FormEvent) => {
    event.preventDefault();
    submit({
      name,
      // Both are omitted rather than sent empty: the API treats an absent
      // field as "not given", and an empty list as "grant nothing".
      description: description || undefined,
      permissions: permissions.length ? permissions : undefined,
    });
  };

  return (
    <div data-testid="page-role-new">
      <PageHeader title={t("role.create")} />
      <PageDescription>{t("role.new-description")}</PageDescription>

      <Card size="sm" className="mb-4 max-w-[44rem]">
        <CardContent>
          <form onSubmit={onSubmit} className="space-y-5">
            <FieldGroup className="gap-4">
              <Field>
                <FieldLabel htmlFor="role-name">
                  {t("common.role-name")}<span className="text-destructive">*</span>
                </FieldLabel>
                <Input
                  id="role-name"
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  placeholder="maven-readers"
                  autoFocus
                  required
                  pattern={NAME_PATTERN}
                  title={t("common.name-rule-64")}
                />
                <FieldDescription>{t("common.name-rule")}</FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="role-description">{t("common.description")}</FieldLabel>
                <Input
                  id="role-description"
                  value={description}
                  onChange={(event) => setDescription(event.target.value)}
                  placeholder={t("common.optional")}
                />
              </Field>
            </FieldGroup>

            <PermissionBuilder permissions={permissions} onChange={setPermissions} />

            {error && <Alert>{error}</Alert>}

            <div className="flex min-w-0 items-center gap-2 border-t border-border pt-4 max-sm:flex-wrap">
              <Button type="submit" disabled={!name.trim() || isPending}>
                {t("role.create")}
              </Button>
              <Button
                variant="outline"
                type="button"
                onClick={() => navigate({ to: "/access/roles" })}
              >
                {t("common.cancel")}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
