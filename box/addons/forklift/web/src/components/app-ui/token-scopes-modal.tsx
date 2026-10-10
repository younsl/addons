import { useState } from "react";
import { Plus, X } from "lucide-react";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { RepositoryPatternCombobox } from "@/components/app/repository-pattern-combobox";
import { Field, FieldLabel } from "@/components/ui/field";
import { useRepositoryPatternOptions } from "@/hooks/repositories/use-repository-pattern-options";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import {
  formatTokenScope,
  parseTokenScopes,
  type TokenScope,
  type TokenScopeActions,
} from "@/utils/token-scopes";

import type { Token } from "@/services/v1/openapi-types";

export type { TokenScope };
export { parseTokenScopes };

// A token scope may only narrow what its owner already holds, so the actions
// offered here are the subset a token can carry - not the full role action list.
// Pinned to the generated type: adding one here that the document does not
// accept would build a scope the API rejects on save.
const ACTIONS = ["read", "write", "delete", "audit"] satisfies TokenScopeActions;

// TokenScopesModal edits the permissions of an existing token in place: scopes
// can be added and removed without re-issuing the secret. Scopes only ever
// narrow the owner's role permissions (enforced server-side at auth time), so
// widening the list here cannot grant access the owner does not already have.
export function TokenScopesModal({ token, onSave, onClose }: {
  token: Token;
  // Persists the full replacement scope list (self-service or admin endpoint).
  onSave: (scopes: TokenScope[]) => Promise<void>;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [scopes, setScopes] = useState<TokenScope[]>(() => parseTokenScopes(token.scopes_json));
  const [pattern, setPattern] = useState("");
  const [actions, setActions] = useState<TokenScopeActions>(["read"]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // Repository names for scope-pattern autocomplete, same source and shape as
  // the token create page and the role permission row.
  const { options: repoOptions, types: repoTypes } = useRepositoryPatternOptions();

  const addScope = () => {
    if (!pattern.trim() || actions.length === 0) return;
    setScopes((cur) => [...cur, { repo_pattern: pattern.trim(), actions: [...actions] }]);
    setPattern("");
    setActions(["read"]);
  };

  const save = async () => {
    setError("");
    setBusy(true);
    try {
      await onSave(scopes);
      onClose();
    } catch (err) {
      setError(getErrorMessage(err));
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-100 flex items-center justify-center bg-black/70 backdrop-blur-[3px]" onClick={onClose}>
      <div className="w-[420px] max-w-[90vw] rounded-lg border border-border bg-card p-5 shadow-[var(--fx-overlay-shadow)]" onClick={(e) => e.stopPropagation()}>
        <h2 className="m-0 mb-1 text-base font-semibold">{t("token.edit-permissions")}</h2>
        <p className="mt-0 mb-4 text-sm text-muted-foreground">
          <span className="font-mono">{token.name}</span> · {t("token.edit-permissions-note")}
        </p>

        <div className="min-h-10 rounded-lg border border-border bg-muted/20 p-2">
          <div className="flex min-w-0 flex-wrap items-center gap-1.5">
            {scopes.map((s, i) => (
              <Badge key={`${s.repo_pattern}-${i}`} className="font-mono">
                {formatTokenScope(s)}
                <Button
                  className="-mr-1 ml-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                  size="icon-xs"
                  variant="ghost"
                  type="button"
                  title={t("common.remove-permission")}
                  onClick={() => setScopes((cur) => cur.filter((_, j) => j !== i))}
                >
                  <X className="size-3" aria-hidden="true" />
                </Button>
              </Badge>
            ))}
            {scopes.length === 0 && (
              <span className="px-1 text-sm text-muted-foreground">{t("common.no-permissions-added")}</span>
            )}
          </div>
        </div>

        <div className="mt-3 rounded-lg border border-border/80 bg-background/40 p-3 space-y-3">
          <Field>
            <FieldLabel>{t("common.repository-pattern")}</FieldLabel>
            <RepositoryPatternCombobox
              className="w-full"
              options={repoOptions}
              types={repoTypes}
              value={pattern}
              onValueChange={setPattern}
            />
          </Field>
          <Field>
            <FieldLabel>{t("common.actions")}</FieldLabel>
            <div className="flex items-center gap-4">
              {ACTIONS.map((a) => (
                <label key={a} className="flex items-center gap-1.5 text-sm">
                  <Checkbox checked={actions.includes(a)}
                    onCheckedChange={(checked) =>
                      setActions((cur) => checked ? [...cur, a] : cur.filter((x) => x !== a))} />
                  <span>{a}</span>
                </label>
              ))}
            </div>
          </Field>
          <div className="flex justify-end">
            <Button variant="outline" type="button" onClick={addScope}
              disabled={!pattern.trim() || actions.length === 0}>
              <Plus data-icon="inline-start" />
              {t("common.add-permission")}
            </Button>
          </div>
        </div>

        {error && <Alert className="mt-3">{error}</Alert>}
        <div className="mt-4 flex min-w-0 items-center justify-end gap-2 max-sm:flex-wrap max-sm:flex-col max-sm:items-stretch">
          <Button variant="outline" type="button" onClick={onClose}>{t("common.cancel")}</Button>
          <Button type="button" disabled={busy || scopes.length === 0}
            title={scopes.length === 0 ? t("token.scopes-required") : undefined}
            onClick={save}>
            {busy ? t("common.saving") : t("common.save")}
          </Button>
        </div>
      </div>
    </div>
  );
}
