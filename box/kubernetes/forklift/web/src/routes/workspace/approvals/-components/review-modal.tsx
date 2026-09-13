import { useState } from "react";

import { Alert } from "@/components/app-ui/alert";
import { ApprovalVulnBadge } from "@/components/app-ui/approval-vuln-badge";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { useDecideApprovalMutation } from "@/routes/workspace/approvals/-hooks/use-approval-mutations";

import type { PackageApproval } from "@/services/v1/openapi-types";

// ReviewModal makes the approve/reject decision with an optional note (in-app,
// never a native dialog). It offers both actions so the reviewer decides in one
// place; the button matching the current status is hidden, since re-approving
// an approved package is a no-op. Shared by the queue and the detail page.
export function ReviewModal({
  row,
  onDone,
  onCancel,
}: {
  row: PackageApproval;
  onDone: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [note, setNote] = useState("");
  const [error, setError] = useState("");
  // Which action is running, not merely that one is: the reviewer pressed a
  // specific button and the label has to say so.
  const [busyDecision, setBusyDecision] = useState<"approve" | "reject" | null>(null);
  const decideMutation = useDecideApprovalMutation();

  const decide = (decision: "approve" | "reject") => {
    setBusyDecision(decision);
    setError("");
    decideMutation.mutate(
      { approvalId: row.id, decision, note },
      {
        onSuccess: onDone,
        onError: (caught) => {
          setError(getErrorMessage(caught));
          setBusyDecision(null);
        },
      },
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
        <h2 className="m-0 mb-3 text-base font-semibold">Review "{row.package}"</h2>
        <p className="mt-0 mb-3 text-sm leading-relaxed text-muted-foreground">
          Approve to serve all versions from {row.repo_name} (age policy still
          applies); reject to block the package, including already-cached content.
        </p>
        <div className="mb-3 flex min-w-0 items-center gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
          <span className="text-sm text-muted-foreground">
            {t("approval.vulnerabilities-label")}
          </span>
          <ApprovalVulnBadge
            severity={row.vuln_severity}
            ids={row.vuln_ids}
            scope={row.vuln_scope}
          />
        </div>
        <Field>
          <FieldLabel>{t("approval.note-optional")}</FieldLabel>
          <Input
            value={note}
            autoFocus
            placeholder={t("approval.reason-placeholder")}
            onChange={(event) => setNote(event.target.value)}
          />
        </Field>
        {error && <Alert className="mt-4">{error}</Alert>}
        <div className="mt-4 flex min-w-0 items-center justify-end gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
          <Button variant="outline" type="button" onClick={onCancel}>
            {t("common.cancel")}
          </Button>
          {row.status !== "rejected" && (
            <Button
              variant="destructive"
              type="button"
              data-testid="action-reject"
              disabled={busyDecision !== null}
              onClick={() => decide("reject")}
            >
              {busyDecision === "reject" ? t("approval.rejecting") : "Reject"}
            </Button>
          )}
          {row.status !== "approved" && (
            <Button
              type="button"
              data-testid="action-approve"
              disabled={busyDecision !== null}
              onClick={() => decide("approve")}
            >
              {busyDecision === "approve" ? "Approving…" : "Approve"}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}
