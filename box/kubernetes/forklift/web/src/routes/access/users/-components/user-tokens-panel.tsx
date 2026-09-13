import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { Badge } from "@/components/app-ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { TokenScopesModal } from "@/components/app-ui/token-scopes-modal";
import { MAX_TOKENS_PER_USER } from "@/components/app-ui/token-quota";
import {
  SortableHead,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
} from "@/components/app-ui/table";
import { useTranslation } from "@/lib/i18n";
import {
  useRevokeUserTokenMutation,
  useUpdateUserTokenScopesMutation,
} from "@/routes/access/users/-hooks/use-user-mutations";
import { formatRelativeTime } from "@/utils/format-relative-time";
import { formatTokenScope, formatTokenScopes, parseTokenScopes } from "@/utils/token-scopes";

import type { Token, User } from "@/services/v1/openapi-types";

// UserTokensPanel shows the user's personal access tokens. Admins can create
// (via the New token page) and revoke them; auditors see the list read-only.
// Token scopes only ever narrow the user's own role permissions, so issuing a
// token here cannot grant access the user does not already have.
export function UserTokensPanel({
  user,
  tokens,
  canWrite,
  runAction,
}: {
  user: User;
  tokens: Token[];
  canWrite: boolean;
  runAction: (action: Promise<unknown>) => void;
}) {
  const { t, language } = useTranslation();
  const navigate = useNavigate();
  const [revokeId, setRevokeId] = useState<number | null>(null);
  const [editing, setEditing] = useState<Token | null>(null);
  const revokeTokenMutation = useRevokeUserTokenMutation();
  const updateScopesMutation = useUpdateUserTokenScopesMutation();
  const { sorted, sort } = useSort(tokens, {
    name: (token) => token.name,
    description: (token) => token.description,
    permissions: (token) => formatTokenScopes(token.scopes_json),
    created: (token) => token.created_at,
    expires: (token) => token.expires_at,
    lastUsed: (token) => token.last_used_at,
  });
  const atCap = tokens.length >= MAX_TOKENS_PER_USER;

  return (
    <Card size="sm" className="mb-4" data-testid="panel-tokens">
      <CardContent>
        <div className="mb-4 flex items-start justify-between gap-3 max-sm:flex-col max-sm:items-stretch">
          <h2 className="m-0 text-base font-semibold">
            {t("token.title")}{" "}
            <span className="text-xs font-normal text-muted-foreground">
              {t("token.subtitle")}
            </span>
          </h2>
          {canWrite && (
            <Button
              disabled={atCap}
              title={atCap ? t("token.limit-reached") : undefined}
              onClick={() =>
                navigate({
                  to: "/access/users/$id/tokens/new",
                  params: { id: String(user.id) },
                })
              }
            >
              {t("token.new")}
            </Button>
          )}
        </div>
        <TableWrap>
          <Table>
            <TableHeader>
              <TableRow>
                <SortableHead k="name" sort={sort}>{t("common.name")}</SortableHead>
                <SortableHead k="description" sort={sort}>{t("common.description")}</SortableHead>
                <SortableHead k="permissions" sort={sort}>{t("common.permissions")}</SortableHead>
                <SortableHead k="created" sort={sort}>{t("common.created")}</SortableHead>
                <SortableHead k="expires" sort={sort}>{t("common.expires")}</SortableHead>
                <SortableHead k="lastUsed" sort={sort}>{t("common.last-used")}</SortableHead>
                {canWrite && <TableHead></TableHead>}
              </TableRow>
            </TableHeader>
            <TableBody>
              {sorted.map((token) => (
                <TableRow key={token.id}>
                  <TableCell>{token.name}</TableCell>
                  <TableCell className="text-muted-foreground">{token.description}</TableCell>
                  <TableCell>
                    {parseTokenScopes(token.scopes_json).map((scope, index) => (
                      <Badge key={index} className="mr-1 font-mono">
                        {formatTokenScope(scope)}
                      </Badge>
                    ))}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {token.created_at?.slice(0, 10)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {token.expires_at ? (
                      <>
                        {token.expires_at.slice(0, 10)}{" "}
                        <span className="text-xs">
                          ({formatRelativeTime(token.expires_at, language)})
                        </span>
                      </>
                    ) : (
                      t("common.never")
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {token.last_used_at ? token.last_used_at.slice(0, 10) : t("common.never")}
                  </TableCell>
                  {canWrite && (
                    <TableCell>
                      <div className="flex items-center justify-end gap-2">
                        <Button variant="outline" onClick={() => setEditing(token)}>
                          {t("common.edit")}
                        </Button>
                        <Button variant="destructive" onClick={() => setRevokeId(token.id)}>
                          {t("token.revoke")}
                        </Button>
                      </div>
                    </TableCell>
                  )}
                </TableRow>
              ))}
              {tokens.length === 0 && (
                <TableRow>
                  <TableCell colSpan={canWrite ? 7 : 6} className="text-muted-foreground">
                    {t("token.empty")}
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </TableWrap>
        {!canWrite && (
          <p className="mb-0 mt-3 text-sm text-muted-foreground">{t("token.readonly-note")}</p>
        )}
        {editing && (
          <TokenScopesModal
            token={editing}
            // Not routed through runAction: the modal catches the rejection and
            // shows it in its own alert, beside the fields being edited. Sending
            // it to the page alert as well would state the failure twice, once
            // behind the modal where it cannot be read.
            onSave={(scopes) =>
              updateScopesMutation.mutateAsync({
                userId: user.id,
                tokenId: editing.id,
                scopes,
              })
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
          onConfirm={() => {
            if (revokeId !== null) {
              runAction(
                revokeTokenMutation.mutateAsync({ userId: user.id, tokenId: revokeId }),
              );
            }
            setRevokeId(null);
          }}
          onCancel={() => setRevokeId(null)}
        />
      </CardContent>
    </Card>
  );
}
