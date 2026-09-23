import {
  addDays,
  generateToken,
  HARD_MAX_EXPIRY_DAYS,
  hashToken,
  isPatToken,
  parseBearer,
  PAT_PREFIX,
  resolveExpiryDays,
  tokenPrefix,
} from './token';

describe('token helpers', () => {
  it('generates prefixed, unique, high-entropy tokens', () => {
    const a = generateToken();
    const b = generateToken();
    expect(a.startsWith(PAT_PREFIX)).toBe(true);
    expect(a).not.toEqual(b);
    expect(a.length).toBe(PAT_PREFIX.length + 43);
    expect(a.slice(PAT_PREFIX.length)).toMatch(/^[A-Za-z0-9]{43}$/);
  });

  it('hashes deterministically with sha256 hex', () => {
    const t = generateToken();
    expect(hashToken(t)).toEqual(hashToken(t));
    expect(hashToken(t)).toMatch(/^[0-9a-f]{64}$/);
    expect(hashToken(t)).not.toEqual(hashToken(`${t}x`));
  });

  it('exposes only a short prefix for display', () => {
    const t = generateToken();
    const prefix = tokenPrefix(t);
    expect(t.startsWith(prefix)).toBe(true);
    expect(prefix.length).toBeLessThan(20);
  });

  it('recognises PAT bearer tokens and ignores JWTs', () => {
    expect(isPatToken(`${PAT_PREFIX}abc`)).toBe(true);
    expect(isPatToken('eyJhbGciOi...')).toBe(false);
    expect(isPatToken(undefined)).toBe(false);
  });

  it('parses bearer headers case-insensitively', () => {
    expect(parseBearer('Bearer abc')).toBe('abc');
    expect(parseBearer('bearer   abc  ')).toBe('abc');
    expect(parseBearer('Basic abc')).toBeUndefined();
    expect(parseBearer(undefined)).toBeUndefined();
  });

  it('rejects bearer headers without a separated token', () => {
    expect(parseBearer('Bearer')).toBeUndefined();
    expect(parseBearer('Bearer    ')).toBeUndefined();
    expect(parseBearer('Bearerabc')).toBeUndefined();
    expect(parseBearer('Bearer\tabc')).toBe('abc');
  });

  describe('resolveExpiryDays', () => {
    it('accepts integers within range', () => {
      expect(resolveExpiryDays(30, 365)).toEqual({ ok: true, days: 30 });
      expect(resolveExpiryDays('90', 365)).toEqual({ ok: true, days: 90 });
    });

    it('rejects values above the configured cap', () => {
      const r = resolveExpiryDays(200, 180);
      expect(r.ok).toBe(false);
      if (!r.ok) expect(r.error).toContain('180');
    });

    it('never allows more than the hard cap even when config is larger', () => {
      expect(resolveExpiryDays(HARD_MAX_EXPIRY_DAYS, 10_000).ok).toBe(true);
      expect(resolveExpiryDays(HARD_MAX_EXPIRY_DAYS + 1, 10_000).ok).toBe(false);
    });

    it('rejects zero, negatives, fractions and garbage', () => {
      expect(resolveExpiryDays(0, 365).ok).toBe(false);
      expect(resolveExpiryDays(-1, 365).ok).toBe(false);
      expect(resolveExpiryDays(1.5, 365).ok).toBe(false);
      expect(resolveExpiryDays('abc', 365).ok).toBe(false);
      expect(resolveExpiryDays(undefined, 365).ok).toBe(false);
    });
  });

  it('adds whole days', () => {
    const from = new Date('2026-01-01T00:00:00Z');
    expect(addDays(from, 10).toISOString()).toBe('2026-01-11T00:00:00.000Z');
  });
});
