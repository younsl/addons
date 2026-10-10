import { useState, type FormEvent } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Select } from "@/components/app-ui/select";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NAME_PATTERN } from "@/lib/name-pattern";
import { useTranslation } from "@/lib/i18n";
import { UserTypePicker, type AccountType } from "@/routes/access/users/-components/user-type-picker";
import { useUserCreateSubmit } from "@/routes/access/users/-hooks/use-user-create-submit";

// Admin-only local user creation, reached from the Create button on
// /access/users. OIDC users are never created here; they appear at first SSO
// login.
export function UserNewPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [accountType, setAccountType] = useState<AccountType>("user");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [isPasswordVisible, setIsPasswordVisible] = useState(false);
  const [email, setEmail] = useState("");
  const [roleId, setRoleId] = useState("");
  const { error, isPending, roles, setError, submit } = useUserCreateSubmit();

  const isRobot = accountType === "robot";
  // Only once something has been typed into the confirmation: flagging a
  // mismatch against an empty field would mark every form in progress as wrong.
  const isMismatch = confirmPassword.length > 0 && password !== confirmPassword;
  const canSubmit = isRobot
    ? Boolean(username.trim())
    : Boolean(username.trim() && password && password === confirmPassword);

  const onSubmit = (event: FormEvent) => {
    event.preventDefault();
    if (!isRobot && password !== confirmPassword) {
      setError(t("user.password-mismatch"));
      return;
    }
    submit({
      username,
      // A robot has no password to set: it authenticates only by token.
      password: isRobot ? undefined : password,
      email: email || undefined,
      role_ids: roleId ? [Number(roleId)] : undefined,
      robot: isRobot || undefined,
    });
  };

  return (
    <div data-testid="page-user-new">
      <PageHeader title={t("user.create-local")} />
      <PageDescription>{t("user.new-description")}</PageDescription>

      <Card size="sm" className="mb-4 max-w-[44rem]">
        <CardContent>
          <form onSubmit={onSubmit} className="space-y-5">
            <FieldGroup className="gap-4">
              <Field>
                <FieldLabel>
                  {t("common.type")}<span className="text-destructive">*</span>
                </FieldLabel>
                <UserTypePicker value={accountType} onChange={setAccountType} />
              </Field>

              <Field>
                <FieldLabel htmlFor="username">
                  {t("common.username")}<span className="text-destructive">*</span>
                </FieldLabel>
                <Input
                  id="username"
                  value={username}
                  onChange={(event) => setUsername(event.target.value)}
                  autoFocus
                  required
                  pattern={NAME_PATTERN}
                  title={t("common.name-rule-64")}
                />
                <FieldDescription>{t("common.name-rule")}</FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="email">{t("common.email")}</FieldLabel>
                <Input
                  id="email"
                  value={email}
                  onChange={(event) => setEmail(event.target.value)}
                  placeholder={t("common.optional")}
                />
                <FieldDescription>{t("user.oidc-note")}</FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="role">{t("common.role")}</FieldLabel>
                <Select
                  value={roleId}
                  onChange={setRoleId}
                  placeholder={t("user.no-role-placeholder")}
                  options={roles.map((role) => ({
                    value: String(role.id),
                    label: role.name,
                    description: role.description || undefined,
                  }))}
                />
                <FieldDescription>{t("user.no-role-note")}</FieldDescription>
              </Field>
            </FieldGroup>

            <div className="space-y-3 border-t border-border pt-4">
              <div>
                <h2 className="m-0 text-sm font-semibold">{t("common.password")}</h2>
                <p className="mt-1 text-sm text-muted-foreground">
                  {isRobot ? t("user.robot-password-note") : t("user.initial-password-note")}
                </p>
              </div>

              {!isRobot && (
                <FieldGroup className="gap-4">
                  <Field>
                    <FieldLabel htmlFor="password">
                      {t("common.password")}<span className="text-destructive">*</span>
                    </FieldLabel>
                    <div className="flex min-w-0 items-stretch gap-2 max-sm:flex-col max-sm:flex-wrap">
                      <Input
                        id="password"
                        // "Confirm password" also matches an accessible-name
                        // lookup for "Password", so both fields are keyed.
                        data-testid="field-password"
                        type={isPasswordVisible ? "text" : "password"}
                        value={password}
                        onChange={(event) => setPassword(event.target.value)}
                        required
                      />
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() => setIsPasswordVisible((current) => !current)}
                        aria-label={
                          isPasswordVisible ? t("common.hide-password") : t("common.show-password")
                        }
                      >
                        {isPasswordVisible ? t("common.hide") : t("common.show")}
                      </Button>
                    </div>
                  </Field>

                  <Field>
                    <FieldLabel htmlFor="confirm-password">
                      {t("common.confirm-password")}<span className="text-destructive">*</span>
                    </FieldLabel>
                    <Input
                      id="confirm-password"
                      data-testid="field-confirm-password"
                      type={isPasswordVisible ? "text" : "password"}
                      value={confirmPassword}
                      onChange={(event) => setConfirmPassword(event.target.value)}
                      required
                      aria-invalid={isMismatch}
                    />
                    {isMismatch && <Alert>{t("user.password-mismatch")}</Alert>}
                  </Field>
                </FieldGroup>
              )}
            </div>

            {error && <Alert>{error}</Alert>}

            <div className="flex min-w-0 items-center gap-2 border-t border-border pt-4 max-sm:flex-wrap">
              <Button type="submit" disabled={!canSubmit || isPending}>
                {t("user.create")}
              </Button>
              <Button
                variant="outline"
                type="button"
                onClick={() => navigate({ to: "/access/users" })}
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
