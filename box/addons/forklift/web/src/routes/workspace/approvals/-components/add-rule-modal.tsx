import { useState, type FormEvent } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/app-ui/select";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import {
  useCreateApprovalMutation,
  useCreateVersionDenyMutation,
} from "@/routes/workspace/approvals/-hooks/use-approval-mutations";

// AddRuleModal records one rule ahead of demand: allow a package (pre-approve),
// block a package, or - when a version is given - block just that version.
// Package-level rules become approval decisions; a version block lands in the
// deny list. Two endpoints behind one form, because to the operator it is one
// decision with a narrower scope.
export function AddRuleModal({
  repoNames,
  initialRepo,
  initialDecision = "allow",
  onDone,
  onCancel,
}: {
  repoNames: string[];
  initialRepo?: string;
  initialDecision?: "allow" | "block";
  onDone: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [repo, setRepo] = useState(initialRepo || (repoNames[0] ?? ""));
  const [packageName, setPackageName] = useState("");
  const [decision, setDecision] = useState<"allow" | "block">(initialDecision);
  const [version, setVersion] = useState("");
  const [note, setNote] = useState("");
  const [error, setError] = useState("");
  const createApprovalMutation = useCreateApprovalMutation();
  const createVersionDenyMutation = useCreateVersionDenyMutation();

  // A version is only meaningful for a block: allowing one version while the
  // package itself is undecided would not narrow anything.
  const isVersionBlock = decision === "block" && version.trim() !== "";
  const isPending =
    createApprovalMutation.isPending || createVersionDenyMutation.isPending;

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setError("");
    const onSettledHandlers = {
      onSuccess: onDone,
      onError: (caught: unknown) => setError(getErrorMessage(caught)),
    };

    if (isVersionBlock) {
      createVersionDenyMutation.mutate(
        { repo, package: packageName.trim(), version: version.trim(), reason: note },
        onSettledHandlers,
      );
      return;
    }

    createApprovalMutation.mutate(
      {
        repo,
        package: packageName.trim(),
        status: decision === "allow" ? "approved" : "rejected",
        note,
      },
      onSettledHandlers,
    );
  };

  return (
    <div
      className="fixed inset-0 z-100 flex items-center justify-center bg-black/70 backdrop-blur-[3px]"
      onClick={onCancel}
    >
      <div
        className="w-[380px] max-w-[90vw] rounded-lg border border-border bg-card p-5 shadow-[var(--fx-overlay-shadow)]"
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="m-0 mb-3 text-base font-semibold">{t("approval.add-rule")}</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          {decision === "allow"
            ? t("approval.rule-allow-note")
            : isVersionBlock
              ? t("approval.rule-block-version-note")
              : t("approval.rule-block-note")}
        </p>
        <form onSubmit={submit} className="space-y-4">
          <Field>
            <FieldLabel>{t("approval.proxy-repository")}</FieldLabel>
            <Select
              value={repo}
              onChange={setRepo}
              options={repoNames.map((name) => ({ value: name, label: name }))}
            />
          </Field>
          <Field>
            <FieldLabel>{t("common.package")}</FieldLabel>
            <Input
              value={packageName}
              placeholder="lodash, @scope/pkg, group:artifact…"
              onChange={(event) => setPackageName(event.target.value)}
            />
          </Field>
          <Field>
            <FieldLabel>{t("common.decision")}</FieldLabel>
            <Select
              value={decision}
              onChange={(next) => setDecision(next as "allow" | "block")}
              options={[
                { value: "allow", label: "allow (pre-approve)" },
                { value: "block", label: "block" },
              ]}
            />
          </Field>
          {decision === "block" && (
            <Field>
              <FieldLabel>{t("approval.version-optional")}</FieldLabel>
              <Input
                value={version}
                placeholder="4.17.99 (go modules: v1.2.3)"
                onChange={(event) => setVersion(event.target.value)}
              />
            </Field>
          )}
          <Field>
            <FieldLabel>{t("approval.note-optional")}</FieldLabel>
            <Input value={note} onChange={(event) => setNote(event.target.value)} />
          </Field>
          {error && <Alert>{error}</Alert>}
          <div className="flex min-w-0 items-center justify-end gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
            <Button variant="outline" type="button" onClick={onCancel}>
              {t("common.cancel")}
            </Button>
            <Button
              variant={decision === "block" ? "destructive" : "default"}
              type="submit"
              data-testid="action-save-rule"
              disabled={!repo || !packageName.trim() || isPending}
            >
              {t("common.save")}
            </Button>
          </div>
        </form>
      </div>
    </div>
  );
}
