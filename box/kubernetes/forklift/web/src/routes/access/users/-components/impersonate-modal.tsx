import { useState } from "react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Textarea } from "@/components/ui/textarea";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { useImpersonateUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { User } from "@/services/v1/openapi-types";

// Minimum reason length accepted by POST /users/{id}/impersonate. Kept in sync
// with the server so the button explains itself before the request is sent.
const MIN_IMPERSONATE_REASON = 10;

// ImpersonateModal collects the justification the server records, then hands the
// browser to the new identity. The session cookie is replaced by the response,
// so the app is reloaded rather than navigated: every cached query belongs to
// the previous identity and must be dropped.
export function ImpersonateModal({ user, onClose }: { user: User; onClose: () => void }) {
  const { t } = useTranslation();
  const [reason, setReason] = useState("");
  const [error, setError] = useState("");
  const impersonateMutation = useImpersonateUserMutation();

  const start = () => {
    setError("");
    impersonateMutation.mutate(
      { userId: user.id, reason: reason.trim() },
      {
        onSuccess: () => window.location.assign("/workspace/repositories"),
        onError: (caught) => setError(getErrorMessage(caught)),
      },
    );
  };

  return (
    <div
      className="fixed inset-0 z-100 flex items-center justify-center bg-black/70 backdrop-blur-[3px]"
      onClick={onClose}
    >
      <div
        className="w-[420px] max-w-[90vw] rounded-lg border border-border bg-card p-5 shadow-[var(--fx-overlay-shadow)]"
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="m-0 mb-1 text-base font-semibold">{t("user.impersonate")}</h2>
        <p className="mb-4 mt-0 text-sm text-muted-foreground">
          <span className="font-mono">{user.username}</span>
          <br />
          {t("user.impersonate-modal-note")}
        </p>
        <Field>
          <FieldLabel>{t("user.impersonate-reason")}</FieldLabel>
          <Textarea
            rows={3}
            value={reason}
            autoFocus
            placeholder={t("user.impersonate-reason-placeholder")}
            onChange={(event) => setReason(event.target.value)}
          />
        </Field>
        <p className="mb-0 mt-2 text-xs text-muted-foreground">
          {t("user.impersonate-reason-hint")}
        </p>
        {error && <Alert className="mt-3">{error}</Alert>}
        <div className="mt-4 flex min-w-0 items-center justify-end gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
          <Button variant="outline" type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button
            variant="destructive"
            type="button"
            disabled={
              impersonateMutation.isPending || reason.trim().length < MIN_IMPERSONATE_REASON
            }
            onClick={start}
          >
            {impersonateMutation.isPending
              ? t("user.impersonate-starting")
              : t("user.impersonate-confirm")}
          </Button>
        </div>
      </div>
    </div>
  );
}
