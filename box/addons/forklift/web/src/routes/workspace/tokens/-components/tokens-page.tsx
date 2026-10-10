import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { QuotasPanel, MAX_TOKENS_PER_USER } from "@/components/app-ui/token-quota";
import { TokenScopesModal } from "@/components/app-ui/token-scopes-modal";
import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { TokensTable } from "@/routes/workspace/tokens/-components/tokens-table";
import {
  useRevokeTokenMutation,
  useTokensList,
  useUpdateTokenScopesMutation,
} from "@/routes/workspace/tokens/-hooks/use-token-mutations";

import type { Token } from "@/services/v1/openapi-types";

// The current user's own access tokens. An admin managing someone else's
// tokens does it from that user's detail page, against a different endpoint.
export function TokensPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [revokeId, setRevokeId] = useState<number | null>(null);
  const [editing, setEditing] = useState<Token | null>(null);
  const [actionError, setActionError] = useState("");
  const tokensQuery = useTokensList();
  const revokeTokenMutation = useRevokeTokenMutation();
  const updateScopesMutation = useUpdateTokenScopesMutation();

  const tokens = tokensQuery.data ?? [];
  const error = actionError || getErrorMessageIfAny(tokensQuery.error);
  const atCap = tokens.length >= MAX_TOKENS_PER_USER;

  const revoke = () => {
    if (revokeId === null) return;
    setActionError("");
    revokeTokenMutation.mutate(revokeId, {
      onError: (caught) => setActionError(getErrorMessage(caught)),
    });
    setRevokeId(null);
  };

  return (
    <div data-testid="page-tokens">
      <PageHeader
        title={t("token.personal-title")}
        actions={
          <Button
            disabled={atCap}
            title={atCap ? t("token.limit-reached") : undefined}
            onClick={() => navigate({ to: "/workspace/tokens/new" })}
          >
            {t("token.new")}
          </Button>
        }
      />
      <PageDescription>
        {t("token.help-1")} <code>_authToken</code>, Maven, Cargo, <code>.netrc</code>{" "}
        {t("token.help-2")}
      </PageDescription>

      {error && <Alert className="mb-4">{error}</Alert>}

      {/* The quotas card is shared with the admin user detail page, so the
          handle tests scope into is attached here rather than inside it. */}
      <div data-testid="panel-quotas">
        <QuotasPanel tokenUsed={tokens.length} />
      </div>

      <TokensTable tokens={tokens} onEdit={setEditing} onRevoke={setRevokeId} />

      {editing && (
        <TokenScopesModal
          token={editing}
          // The modal shows its own failure beside the fields being edited, so
          // this is not routed through the page alert as well.
          onSave={(scopes) =>
            updateScopesMutation.mutateAsync({ tokenId: editing.id, scopes })
          }
          onClose={() => setEditing(null)}
        />
      )}

      <ConfirmModal
        open={revokeId !== null}
        title={t("token.revoke-confirm-title")}
        message={t("token.revoke-confirm-message")}
        confirmLabel={t("token.revoke")}
        danger
        onConfirm={revoke}
        onCancel={() => setRevokeId(null)}
      />
    </div>
  );
}
