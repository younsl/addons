import {
  accessForMethod,
  checkScope,
  decodeScopes,
  encodeScopes,
  normalizeScopes,
  pluginIdFromPath,
} from './scopes';

const allowed = new Set(['catalog', 'search', 'opencost']);

describe('scopes', () => {
  it('maps HTTP methods to access levels', () => {
    expect(accessForMethod('GET')).toBe('read');
    expect(accessForMethod('head')).toBe('read');
    expect(accessForMethod('OPTIONS')).toBe('read');
    expect(accessForMethod('POST')).toBe('write');
    expect(accessForMethod('DELETE')).toBe('write');
  });

  describe('normalizeScopes', () => {
    it('sorts and dedupes, keeping the highest access', () => {
      const r = normalizeScopes(
        [
          { plugin: 'search', access: 'read' },
          { plugin: 'catalog', access: 'read' },
          { plugin: 'catalog', access: 'write' },
        ],
        allowed,
      );
      expect(r).toEqual({
        ok: true,
        scopes: [
          { plugin: 'catalog', access: 'write' },
          { plugin: 'search', access: 'read' },
        ],
      });
    });

    it('rejects empty input', () => {
      expect(normalizeScopes([], allowed).ok).toBe(false);
      expect(normalizeScopes(undefined, allowed).ok).toBe(false);
    });

    it('rejects plugins outside the allow list', () => {
      const r = normalizeScopes([{ plugin: 'scaffolder', access: 'read' }], allowed);
      expect(r.ok).toBe(false);
      if (!r.ok) expect(r.error).toContain('not scopable');
    });

    it('never allows the pat plugin itself even when listed', () => {
      const r = normalizeScopes(
        [{ plugin: 'pat', access: 'read' }],
        new Set(['pat', 'catalog']),
      );
      expect(r.ok).toBe(false);
    });

    it('rejects invalid access values and malformed ids', () => {
      expect(normalizeScopes([{ plugin: 'catalog', access: 'admin' }], allowed).ok).toBe(false);
      expect(normalizeScopes([{ plugin: 'Cat alog', access: 'read' }], allowed).ok).toBe(false);
      expect(normalizeScopes(['catalog'], allowed).ok).toBe(false);
    });
  });

  describe('checkScope', () => {
    const scopes = [
      { plugin: 'catalog', access: 'read' as const },
      { plugin: 'opencost', access: 'write' as const },
    ];

    it('allows reads on a read scope', () => {
      expect(checkScope(scopes, 'catalog', 'GET').allowed).toBe(true);
    });

    it('denies writes on a read scope', () => {
      expect(checkScope(scopes, 'catalog', 'POST')).toEqual({
        allowed: false,
        reason: 'scope_insufficient',
      });
    });

    it('allows any method on a write scope', () => {
      expect(checkScope(scopes, 'opencost', 'DELETE').allowed).toBe(true);
      expect(checkScope(scopes, 'opencost', 'GET').allowed).toBe(true);
    });

    it('denies plugins without a scope', () => {
      expect(checkScope(scopes, 'search', 'GET')).toEqual({
        allowed: false,
        reason: 'scope_missing',
      });
    });

    it('always denies the pat plugin', () => {
      expect(checkScope([{ plugin: 'pat', access: 'write' }], 'pat', 'GET')).toEqual({
        allowed: false,
        reason: 'plugin_forbidden',
      });
    });
  });

  it('round-trips through storage and drops malformed entries', () => {
    const scopes = [{ plugin: 'catalog', access: 'read' as const }];
    expect(decodeScopes(encodeScopes(scopes))).toEqual(scopes);
    expect(decodeScopes('[{"plugin":"x","access":"root"},{"plugin":"y","access":"write"}]')).toEqual([
      { plugin: 'y', access: 'write' },
    ]);
    expect(decodeScopes('not json')).toEqual([]);
    expect(decodeScopes(null)).toEqual([]);
  });

  it('extracts the plugin id from backend paths', () => {
    expect(pluginIdFromPath('/api/catalog/entities?filter=kind=component')).toBe('catalog');
    expect(pluginIdFromPath('/api/opencost')).toBe('opencost');
    expect(pluginIdFromPath('/healthcheck')).toBeUndefined();
    expect(pluginIdFromPath('/api/')).toBeUndefined();
  });
});
