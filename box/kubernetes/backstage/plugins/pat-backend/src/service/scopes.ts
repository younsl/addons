import { ScopeAccess, TokenScope } from './types';

/** The PAT plugin can never be a scope target: a token must not manage tokens. */
export const SELF_PLUGIN_ID = 'pat';

const PLUGIN_ID_PATTERN = /^[a-z0-9][a-z0-9-]{0,63}$/;
const READ_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

export function accessForMethod(method: string): ScopeAccess {
  return READ_METHODS.has(method.toUpperCase()) ? 'read' : 'write';
}

/**
 * Validates and normalises raw scope input from the admin API.
 * Duplicate plugins collapse to the highest access level.
 */
export function normalizeScopes(
  raw: unknown,
  allowedPlugins: ReadonlySet<string>,
): { ok: true; scopes: TokenScope[] } | { ok: false; error: string } {
  if (!Array.isArray(raw) || raw.length === 0) {
    return { ok: false, error: 'scopes must be a non-empty array' };
  }
  const byPlugin = new Map<string, ScopeAccess>();
  for (const item of raw) {
    if (!item || typeof item !== 'object') {
      return { ok: false, error: 'each scope must be an object' };
    }
    const plugin = String((item as any).plugin ?? '').trim();
    const access = String((item as any).access ?? '').trim() as ScopeAccess;
    if (!PLUGIN_ID_PATTERN.test(plugin)) {
      return { ok: false, error: `invalid plugin id: "${plugin}"` };
    }
    if (plugin === SELF_PLUGIN_ID) {
      return { ok: false, error: 'the pat plugin cannot be granted as a scope' };
    }
    if (!allowedPlugins.has(plugin)) {
      return { ok: false, error: `plugin "${plugin}" is not scopable` };
    }
    if (access !== 'read' && access !== 'write') {
      return { ok: false, error: `invalid access "${access}" for ${plugin}` };
    }
    const existing = byPlugin.get(plugin);
    if (!existing || (existing === 'read' && access === 'write')) {
      byPlugin.set(plugin, access);
    }
  }
  const scopes = [...byPlugin.entries()]
    .map(([plugin, access]) => ({ plugin, access }))
    .sort((a, b) => a.plugin.localeCompare(b.plugin));
  return { ok: true, scopes };
}

export type ScopeCheck =
  | { allowed: true; scope: TokenScope }
  | { allowed: false; reason: 'plugin_forbidden' | 'scope_missing' | 'scope_insufficient' };

/** Decides whether `scopes` permit `method` against `pluginId`. */
export function checkScope(
  scopes: readonly TokenScope[],
  pluginId: string,
  method: string,
): ScopeCheck {
  if (pluginId === SELF_PLUGIN_ID) {
    return { allowed: false, reason: 'plugin_forbidden' };
  }
  const scope = scopes.find(s => s.plugin === pluginId);
  if (!scope) {
    return { allowed: false, reason: 'scope_missing' };
  }
  if (accessForMethod(method) === 'write' && scope.access !== 'write') {
    return { allowed: false, reason: 'scope_insufficient' };
  }
  return { allowed: true, scope };
}

/** Serialises scopes for storage. */
export function encodeScopes(scopes: readonly TokenScope[]): string {
  return JSON.stringify(scopes);
}

/** Parses stored scopes, dropping anything malformed rather than failing the row. */
export function decodeScopes(value: unknown): TokenScope[] {
  if (typeof value !== 'string') return [];
  try {
    const parsed = JSON.parse(value);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter(
        (s: any) =>
          s &&
          typeof s.plugin === 'string' &&
          (s.access === 'read' || s.access === 'write'),
      )
      .map((s: any) => ({ plugin: s.plugin, access: s.access }));
  } catch {
    return [];
  }
}

/** Extracts the target plugin id from a backend URL path such as `/api/catalog/entities`. */
export function pluginIdFromPath(path: string): string | undefined {
  const match = /^\/api\/([^/?#]+)/.exec(path);
  return match ? match[1] : undefined;
}
