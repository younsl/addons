import { createHash, randomBytes, timingSafeEqual } from 'crypto';

/** Every PAT starts with this marker so the gateway can tell it apart from a Backstage JWT. */
export const PAT_PREFIX = 'bs_';

/** Hard ceiling on token lifetime. Config may lower it, never raise it. */
export const HARD_MAX_EXPIRY_DAYS = 365;

/** 43 base62 characters carry just over 256 bits of entropy. */
const SECRET_LENGTH = 43;
const ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
const DISPLAY_PREFIX_LENGTH = PAT_PREFIX.length + 8;

/**
 * Alphanumeric secret with no `_` or `-`, so the token reads as one word and
 * the separator after the prefix is unambiguous. Rejection sampling keeps
 * the distribution uniform over the alphabet.
 */
function randomBase62(length: number): string {
  const out: string[] = [];
  const limit = 256 - (256 % ALPHABET.length);
  while (out.length < length) {
    for (const byte of randomBytes(length)) {
      if (byte < limit) {
        out.push(ALPHABET[byte % ALPHABET.length]);
        if (out.length === length) break;
      }
    }
  }
  return out.join('');
}

export function generateToken(): string {
  return `${PAT_PREFIX}${randomBase62(SECRET_LENGTH)}`;
}

export function hashToken(token: string): string {
  return createHash('sha256').update(token, 'utf8').digest('hex');
}

/** Short, non-secret head of the token used to identify it in lists and logs. */
export function tokenPrefix(token: string): string {
  return token.slice(0, DISPLAY_PREFIX_LENGTH);
}

export function isPatToken(value: string | undefined): value is string {
  return typeof value === 'string' && value.startsWith(PAT_PREFIX);
}

const BEARER_SCHEME = 'bearer';

/**
 * Extracts a bearer token from an Authorization header, or undefined when absent.
 * Parsed by slicing rather than a `\s+(.+)` regex, which backtracks polynomially
 * on a header of `Bearer` followed by a long run of whitespace.
 */
export function parseBearer(header: string | undefined): string | undefined {
  if (!header) return undefined;
  const value = header.trim();
  if (value.slice(0, BEARER_SCHEME.length).toLowerCase() !== BEARER_SCHEME) return undefined;
  const rest = value.slice(BEARER_SCHEME.length);
  if (!/^\s/.test(rest)) return undefined;
  const token = rest.trim();
  return token || undefined;
}

export function hashesEqual(a: string, b: string): boolean {
  const ab = Buffer.from(a, 'hex');
  const bb = Buffer.from(b, 'hex');
  if (ab.length !== bb.length) return false;
  return timingSafeEqual(ab, bb);
}

/** Clamps a requested lifetime into `[1, maxDays]`, rejecting non-numeric input. */
export function resolveExpiryDays(
  requested: unknown,
  maxDays: number,
): { ok: true; days: number } | { ok: false; error: string } {
  const cap = Math.min(Math.max(1, Math.floor(maxDays)), HARD_MAX_EXPIRY_DAYS);
  const n = typeof requested === 'number' ? requested : Number(requested);
  if (!Number.isFinite(n) || !Number.isInteger(n)) {
    return { ok: false, error: 'expiresInDays must be an integer' };
  }
  if (n < 1) {
    return { ok: false, error: 'expiresInDays must be at least 1' };
  }
  if (n > cap) {
    return { ok: false, error: `expiresInDays must not exceed ${cap}` };
  }
  return { ok: true, days: n };
}

export function addDays(from: Date, days: number): Date {
  return new Date(from.getTime() + days * 86_400_000);
}
