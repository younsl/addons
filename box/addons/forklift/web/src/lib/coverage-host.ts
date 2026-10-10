// Client-side mirror of the forklift-host rule in src/coverage/host.rs, so
// the Other domains field can mark each entry as it is typed instead of only
// reporting a rejection after Save.
//
// The server stays the authority: this exists to give the field an immediate
// answer, and coverage-host.test.ts pins the same cases as the Rust test so the
// two cannot drift apart unnoticed.

// Reason codes, resolved to copy by the caller so the messages stay in the
// locale files.
export type HostProblem =
  | "required"
  | "not-bare"
  | "port-range"
  | "too-long"
  | "not-external";

const LABEL_RE = /^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/;
const TLD_RE = /^[A-Za-z]{2,63}$/;

const MAX_HOST_LENGTH = 253;
const MAX_PORT = 65535;

// Suffixes that never resolve on the public internet. A forklift is reached at
// an external domain, so a name ending in one of these cannot be the address a
// build resolves through.
const RESERVED_TLDS = new Set([
  "local",
  "localhost",
  "internal",
  "svc",
  "cluster",
  "arpa",
  "home",
  "lan",
  "intranet",
  "test",
  "invalid",
  "example",
  "onion",
]);

// normalizeHost strips a scheme and trailing slashes, leaving "host" or
// "host:port".
export function normalizeHost(raw: string): string {
  let host = raw.trim();
  for (const prefix of ["https://", "http://"]) {
    if (host.slice(0, prefix.length).toLowerCase() === prefix) {
      host = host.slice(prefix.length);
      break;
    }
  }
  return host.replace(/\/+$/, "");
}

// validateExternalDomain reports why raw cannot be a forklift domain, or null
// when it can.
export function validateExternalDomain(raw: string): HostProblem | null {
  const host = normalizeHost(raw);
  if (!host) return "required";
  if (/[/@?#*,\s]/.test(host)) return "not-bare";

  const colon = host.indexOf(":");
  const name = colon >= 0 ? host.slice(0, colon) : host;
  if (colon >= 0) {
    const port = Number(host.slice(colon + 1));
    if (!Number.isInteger(port) || port < 1 || port > MAX_PORT) return "port-range";
  }
  if (!name || name.length > MAX_HOST_LENGTH) return "too-long";

  const labels = name.split(".");
  if (labels.length < 2) return "not-external";
  const tld = labels[labels.length - 1];
  if (!TLD_RE.test(tld) || RESERVED_TLDS.has(tld.toLowerCase())) return "not-external";
  if (!labels.every((label) => LABEL_RE.test(label))) return "not-external";
  return null;
}
