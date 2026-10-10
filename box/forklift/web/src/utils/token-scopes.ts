// A token's scopes travel as a JSON string in scopes_json rather than as a
// structured field, so every screen that shows a token has to parse it. This
// was copied into the user detail page and the tokens page independently; it
// lives here so the two cannot drift.

import type { TokenScope } from "@/services/v1/openapi-types";

// The generated type, not a local restatement: its action list is a union the
// document fixes, and a hand-written `string[]` would let a screen build a
// scope the API rejects.
export type { TokenScope };

export type TokenScopeActions = TokenScope["actions"];

// A token predating the scopes feature, or one written by an older server, may
// hold anything at all here. A malformed value means "no scopes recorded", not
// a broken page - so this never throws.
export function parseTokenScopes(json: string): TokenScope[] {
  try {
    const parsed = JSON.parse(json);
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

export function formatTokenScope(scope: TokenScope): string {
  return `${scope.repo_pattern}: ${scope.actions.join(",")}`;
}

export function formatTokenScopes(json: string): string {
  return parseTokenScopes(json).map(formatTokenScope).join(", ");
}
