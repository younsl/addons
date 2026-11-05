import type { UpstreamAuthConfig } from "@/api";
import { Select } from "@/components/app-ui/select";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useTranslation } from "@/lib/i18n";

// UpstreamAuthFields edits a proxy's upstream credentials, shared by the New
// repository form and the repository Settings tab. Secret fields arrive
// masked from the API on existing repositories; leaving the mask untouched
// keeps the stored secret, typing a new value replaces it.
export function UpstreamAuthFields({ value, onChange }: {
  value: UpstreamAuthConfig;
  onChange: (next: UpstreamAuthConfig) => void;
}) {
  const { t } = useTranslation();
  const type = value.type ?? "";
  return (
    <div>
      <div className="mb-3">
        <h3 className="m-0 text-xs font-semibold uppercase tracking-normal text-muted-foreground">{t("repo.upstream-auth")}</h3>
        <p className="mb-0 mt-1 text-xs leading-5 text-muted-foreground">{t("repo.upstream-auth-desc")}</p>
      </div>
      <FieldGroup className="grid gap-3 md:grid-cols-3">
        <Field>
          <FieldLabel>{t("repo.upstream-auth-method")}</FieldLabel>
          <Select value={type} options={[
            { value: "", label: t("repo.upstream-auth-none") },
            { value: "basic", label: "Basic (username/password)" },
            { value: "bearer", label: "Bearer token" },
            { value: "header", label: t("repo.upstream-auth-header") },
          ]} onChange={(next) => onChange(next === "" ? {} : { type: next as UpstreamAuthConfig["type"] })} />
        </Field>
        {type === "basic" && (
          <>
            <Field>
              <FieldLabel>{t("common.username")}</FieldLabel>
              <Input value={value.username ?? ""} autoComplete="off"
                onChange={(e) => onChange({ ...value, username: e.target.value })} />
            </Field>
            <Field>
              <FieldLabel>{t("common.password")}</FieldLabel>
              <Input type="password" value={value.password ?? ""} autoComplete="new-password"
                onChange={(e) => onChange({ ...value, password: e.target.value })} />
            </Field>
          </>
        )}
        {type === "bearer" && (
          <Field className="md:col-span-2">
            <FieldLabel>{t("repo.upstream-auth-token")}</FieldLabel>
            <Input type="password" value={value.token ?? ""} autoComplete="off"
              onChange={(e) => onChange({ ...value, token: e.target.value })} />
          </Field>
        )}
        {type === "header" && (
          <>
            <Field>
              <FieldLabel>{t("repo.upstream-auth-header-name")}</FieldLabel>
              <Input value={value.header ?? ""} placeholder="X-Api-Key" autoComplete="off"
                onChange={(e) => onChange({ ...value, header: e.target.value })} />
            </Field>
            <Field>
              <FieldLabel>{t("repo.upstream-auth-header-value")}</FieldLabel>
              <Input type="password" value={value.value ?? ""} autoComplete="off"
                onChange={(e) => onChange({ ...value, value: e.target.value })} />
            </Field>
          </>
        )}
      </FieldGroup>
      {type !== "" && (
        <p className="mb-0 mt-2 text-xs leading-5 text-muted-foreground">{t("repo.upstream-auth-note")}</p>
      )}
    </div>
  );
}
