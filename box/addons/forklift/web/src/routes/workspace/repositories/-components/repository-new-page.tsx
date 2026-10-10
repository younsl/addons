import type { FormEvent } from "react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { UpstreamAuthFields } from "@/components/app-ui/upstream-auth-fields";
import { NAME_PATTERN } from "@/lib/name-pattern";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { ConnectivityHint } from "@/routes/workspace/repositories/-components/connectivity-hint";
import { MemberList } from "@/routes/workspace/repositories/-components/member-list";
import { SelectControl } from "@/routes/workspace/repositories/-components/select-control";
import {
  useRepositoryCreateForm,
  type RepositoryFormat,
  type RepositoryType,
} from "@/routes/workspace/repositories/-hooks/use-repository-create-form";

const REPO_TYPES = [
  { value: "hosted", titleKey: "repo.type.hosted", descKey: "repo.type.hosted-desc" },
  { value: "proxy", titleKey: "repo.type.proxy", descKey: "repo.type.proxy-desc" },
  { value: "group", titleKey: "repo.type.group", descKey: "repo.type.group-desc" },
] as const;

const VISIBILITIES = [
  { isPublic: false, titleKey: "repo.private", descKey: "repo.visibility-private-desc" },
  { isPublic: true, titleKey: "repo.public", descKey: "repo.visibility-public-desc" },
] as const;

const FORMATS = [
  { value: "maven", label: "Maven / Gradle" },
  { value: "npm", label: "npm" },
  { value: "cargo", label: "Cargo" },
  { value: "go", label: "Go Modules" },
  { value: "pypi", label: "PyPI" },
  { value: "raw", label: "Raw" },
  { value: "oci", label: "OCI (Container / Helm)" },
];

export function RepositoryNewPage() {
  const { t } = useTranslation();
  const form = useRepositoryCreateForm();

  const onSubmit = (event: FormEvent) => {
    event.preventDefault();
    form.submit();
  };

  return (
    <>
      <header className="mb-4">
        <h1 className="m-0 text-2xl font-semibold tracking-normal">{t("repo.new")}</h1>
        <p className="mt-1 max-w-[58rem] text-sm leading-relaxed text-muted-foreground">
          {t("repo.new-description")}
        </p>
      </header>

      <Card size="sm" className="mb-4 max-w-[52rem]">
        <CardContent>
          <form onSubmit={onSubmit} className="space-y-5">
            <section>
              <div className="mb-4">
                <h2 className="m-0 text-base font-semibold">{t("repo.basics")}</h2>
                <p className="mt-1 text-sm text-muted-foreground">{t("repo.basics-description")}</p>
              </div>

              <FieldGroup className="gap-4">
                <Field>
                  <FieldLabel htmlFor="repository-name">
                    {t("common.name")}<span className="text-destructive">*</span>
                  </FieldLabel>
                  <Input
                    id="repository-name"
                    value={form.name}
                    onChange={(event) => form.setName(event.target.value)}
                    placeholder="maven-central"
                    required
                    autoFocus
                    pattern={NAME_PATTERN}
                    title={t("common.name-rule-64")}
                  />
                  <FieldDescription>{t("common.name-rule")}</FieldDescription>
                </Field>

                <Field>
                  <FieldLabel htmlFor="repository-description">{t("common.description")}</FieldLabel>
                  <Input
                    id="repository-description"
                    value={form.description}
                    onChange={(event) => form.setDescription(event.target.value)}
                    placeholder={t("repo.description-placeholder")}
                    maxLength={1000}
                  />
                  <FieldDescription>{t("repo.description-hint")}</FieldDescription>
                </Field>

                <Field>
                  <FieldLabel>
                    {t("common.format")}<span className="text-destructive">*</span>
                  </FieldLabel>
                  <SelectControl
                    value={form.format}
                    onChange={(next) => form.setFormat(next as RepositoryFormat)}
                    options={FORMATS}
                  />
                </Field>

                <Field>
                  <FieldLabel>
                    {t("common.type")}<span className="text-destructive">*</span>
                  </FieldLabel>
                  <div
                    className="grid gap-2 md:grid-cols-3"
                    role="radiogroup"
                    aria-label="Repository type"
                  >
                    {REPO_TYPES.map((repoType) => {
                      const isSelected = form.type === repoType.value;

                      return (
                        <Button
                          key={repoType.value}
                          type="button"
                          variant="ghost"
                          role="radio"
                          aria-checked={isSelected}
                          className={cn(
                            // One background class applies at a time, and the
                            // unselected card is not dimmed -- opacity-55 put its
                            // description at 2.5:1, and these are options that have
                            // to be read to be chosen, not disabled controls.
                            "h-full w-full flex-col items-start justify-start whitespace-normal rounded-lg border px-3.5 py-3 text-left text-sm transition-all",
                            isSelected
                              ? "border-accent-ink bg-primary/10"
                              : "border-border bg-muted hover:bg-[var(--fx-surface-3)]",
                          )}
                          onClick={() => form.setType(repoType.value as RepositoryType)}
                        >
                          <div className={cn("mb-1 font-semibold", isSelected && "text-accent-ink")}>
                            {t(repoType.titleKey)}
                          </div>
                          <div className="text-xs leading-relaxed text-muted-foreground">
                            {t(repoType.descKey)}
                          </div>
                        </Button>
                      );
                    })}
                  </div>
                </Field>

                <Field>
                  <FieldLabel>{t("repo.visibility")}</FieldLabel>
                  <div
                    className="grid gap-2 md:grid-cols-2"
                    role="radiogroup"
                    aria-label={t("repo.visibility")}
                  >
                    {VISIBILITIES.map((visibility) => {
                      const isSelected = form.isPublic === visibility.isPublic;

                      return (
                        <Button
                          key={String(visibility.isPublic)}
                          type="button"
                          variant="ghost"
                          role="radio"
                          aria-checked={isSelected}
                          className={cn(
                            // One background class applies at a time, and the
                            // unselected card is not dimmed -- opacity-55 put its
                            // description at 2.5:1, and these are options that have
                            // to be read to be chosen, not disabled controls.
                            "h-full w-full flex-col items-start justify-start whitespace-normal rounded-lg border px-3.5 py-3 text-left text-sm transition-all",
                            isSelected
                              ? "border-accent-ink bg-primary/10"
                              : "border-border bg-muted hover:bg-[var(--fx-surface-3)]",
                          )}
                          onClick={() => form.setIsPublic(visibility.isPublic)}
                        >
                          <div className={cn("mb-1 font-semibold", isSelected && "text-accent-ink")}>
                            {visibility.isPublic
                              ? t(visibility.titleKey)
                              : `${t(visibility.titleKey)} (${t("common.default")})`}
                          </div>
                          <div className="text-xs leading-relaxed text-muted-foreground">
                            {t(visibility.descKey)}
                          </div>
                        </Button>
                      );
                    })}
                  </div>
                </Field>
              </FieldGroup>
            </section>

            {form.type === "proxy" && (
              <section className="border-t border-border pt-5">
                <div className="mb-4">
                  <h2 className="m-0 text-base font-semibold">{t("repo.proxy-upstream")}</h2>
                  <p className="mt-1 text-sm text-muted-foreground">{t("repo.proxy-description")}</p>
                </div>

                <FieldGroup className="gap-4">
                  <Field>
                    <FieldLabel htmlFor="upstream-url">
                      {t("repo.upstream-url")}<span className="text-destructive">*</span>
                    </FieldLabel>
                    <Input
                      id="upstream-url"
                      value={form.upstreamUrl}
                      onChange={(event) => form.setUpstreamUrl(event.target.value)}
                      placeholder="https://repo1.maven.org/maven2"
                      required
                    />
                    <ConnectivityHint
                      isChecking={form.upstreamCheck.isChecking}
                      health={form.upstreamCheck.health}
                      hasUrl={form.upstreamCheck.hasUrl}
                    />
                  </Field>

                  <UpstreamAuthFields value={form.upstreamAuth} onChange={form.setUpstreamAuth} />

                  <Field>
                    <FieldLabel>{t("repo.age-policy")}</FieldLabel>
                    <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
                      <Checkbox
                        checked={form.isAgeEnabled}
                        onCheckedChange={(checked) => form.setIsAgeEnabled(checked === true)}
                        aria-label={t("repo.cooldown-label")}
                      />
                      <span>{t("repo.cooldown-label")}</span>
                    </label>
                    <FieldDescription>{t("repo.cooldown-note")}</FieldDescription>
                  </Field>

                  {form.isAgeEnabled && (
                    <Field>
                      <FieldLabel htmlFor="minimum-age">
                        {t("repo.minimum-age")}<span className="text-destructive">*</span>
                      </FieldLabel>
                      <Input
                        id="minimum-age"
                        value={form.minAge}
                        onChange={(event) => form.setMinAge(event.target.value)}
                        placeholder="3d"
                        required
                      />
                      <FieldDescription>{t("repo.cooldown-examples")}</FieldDescription>
                    </Field>
                  )}
                </FieldGroup>
              </section>
            )}

            {form.type === "group" && (
              <section className="border-t border-border pt-5">
                <div className="mb-4">
                  <h2 className="m-0 text-base font-semibold">{t("repo.members")}</h2>
                  <p className="mt-1 text-sm text-muted-foreground">{t("repo.members-note")}</p>
                </div>

                <MemberList
                  members={form.members}
                  onChange={form.setMembers}
                  repoIndex={Object.fromEntries(form.repositories.map((r) => [r.name, r.id]))}
                  repoTypes={Object.fromEntries(form.repositories.map((r) => [r.name, r.type]))}
                />
                <div className="mt-3 flex min-w-0 items-center gap-2 max-sm:flex-wrap">
                  <SelectControl
                    value=""
                    placeholder={t("repo.add-member-placeholder")}
                    onChange={(next) => next && form.setMembers([...form.members, next])}
                    options={form.candidates.map((repository) => ({
                      value: repository.name,
                      label: `${repository.name} (${repository.type})`,
                    }))}
                  />
                </div>
                {form.candidates.length === 0 && form.members.length === 0 && (
                  <p className="mt-3 text-sm text-muted-foreground">
                    No {form.format} repositories exist yet. Create the members first.
                  </p>
                )}
              </section>
            )}

            {form.error && (
              <Alert variant="destructive">
                <AlertDescription>{form.error}</AlertDescription>
              </Alert>
            )}

            <div className="flex min-w-0 items-center gap-2 border-t border-border pt-5 max-sm:flex-col max-sm:items-stretch">
              <Button type="submit" disabled={!form.isComplete || form.isCreating}>
                {t("repo.create")}
              </Button>
              <Button variant="outline" type="button" onClick={form.cancel}>
                {t("common.cancel")}
              </Button>
            </div>
          </form>
        </CardContent>
      </Card>
    </>
  );
}
