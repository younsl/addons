import { useState } from "react";
import { Plus, X } from "lucide-react";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import {
  Combobox,
  ComboboxChip,
  ComboboxChips,
  ComboboxChipsInput,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxItem,
  ComboboxList,
  useComboboxAnchor,
} from "@/components/ui/combobox";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { RepositoryPatternCombobox } from "@/components/app/repository-pattern-combobox";
import { useRepositoryPatternOptions } from "@/hooks/repositories/use-repository-pattern-options";
import { useTranslation } from "@/lib/i18n";
import { formatTokenScope, type TokenScope, type TokenScopeActions } from "@/utils/token-scopes";

// The actions a token may carry. Narrower than a role's grantable actions - a
// scope can only ever restrict what its owner already holds - and pinned to the
// generated type so a value the API rejects cannot be offered.
const TOKEN_ACTIONS = ["read", "write", "delete", "audit"] satisfies TokenScopeActions;

// Collects scopes before the token exists, so nothing here calls the API: the
// list goes out with the create.
export function TokenScopeBuilder({
  scopes,
  onChange,
}: {
  scopes: TokenScope[];
  onChange: (scopes: TokenScope[]) => void;
}) {
  const { t } = useTranslation();
  const [pattern, setPattern] = useState("");
  const [actions, setActions] = useState<TokenScopeActions>(["read"]);
  const [actionSearch, setActionSearch] = useState("");
  const actionAnchorRef = useComboboxAnchor();
  const { options: repoOptions, types: repoTypes } = useRepositoryPatternOptions();
  const actionOptions = TOKEN_ACTIONS.filter((action) =>
    action.includes(actionSearch.trim().toLowerCase()),
  );

  const add = () => {
    if (!pattern.trim() || actions.length === 0) return;
    onChange([...scopes, { repo_pattern: pattern.trim(), actions: [...actions] }]);
    setPattern("");
    setActions(["read"]);
    setActionSearch("");
  };

  return (
    <div className="space-y-3 border-t border-border pt-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 className="m-0 text-sm font-semibold">{t("common.permissions")}</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            {t("token.permissions-description")}
          </p>
        </div>
        <Badge className="mt-0.5">{scopes.length}</Badge>
      </div>

      <div className="min-h-10 rounded-lg border border-border bg-muted/20 p-2">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          {scopes.map((scope, index) => (
            // Nothing has an id yet, and the same pattern may legitimately be
            // added twice with different actions, so the index is in the key.
            <Badge key={`${scope.repo_pattern}-${index}`} className="font-mono">
              {formatTokenScope(scope)}
              <Button
                className="-mr-1 ml-1 size-4 rounded-full text-muted-foreground hover:bg-background/40 hover:text-foreground"
                size="icon-xs"
                variant="ghost"
                type="button"
                title={t("common.remove-permission")}
                onClick={() => onChange(scopes.filter((_, other) => other !== index))}
              >
                <X className="size-3" aria-hidden="true" />
              </Button>
            </Badge>
          ))}
          {scopes.length === 0 && (
            <span className="px-1 text-sm text-muted-foreground">
              {t("common.no-permissions-added")}
            </span>
          )}
        </div>
      </div>

      <div className="rounded-lg border border-border/80 bg-background/40 p-3">
        <FieldGroup className="gap-3">
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
            <Combobox
              multiple
              items={actionOptions}
              inputValue={actionSearch}
              value={actions}
              onInputValueChange={setActionSearch}
              onValueChange={(next) => {
                setActions(next);
                setActionSearch("");
              }}
            >
              <ComboboxChips ref={actionAnchorRef} className="w-full">
                {actions.map((action) => (
                  <ComboboxChip key={action}>{action}</ComboboxChip>
                ))}
                <ComboboxChipsInput
                  placeholder={actions.length ? t("common.add-action") : t("common.select-actions")}
                />
              </ComboboxChips>
              <ComboboxContent anchor={actionAnchorRef}>
                <ComboboxEmpty>{t("common.no-actions-found")}</ComboboxEmpty>
                <ComboboxList>
                  {actionOptions.map((action) => (
                    <ComboboxItem key={action} value={action}>
                      {action}
                    </ComboboxItem>
                  ))}
                </ComboboxList>
              </ComboboxContent>
            </Combobox>
          </Field>

          <div className="flex justify-end">
            <Button
              variant="outline"
              type="button"
              onClick={add}
              disabled={!pattern.trim() || actions.length === 0}
            >
              <Plus data-icon="inline-start" />
              {t("common.add-permission")}
            </Button>
          </div>
        </FieldGroup>
      </div>
    </div>
  );
}
