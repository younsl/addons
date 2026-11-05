// block) to its first/last address and how many addresses it spans, so the UI
// can show the range and count a CIDR expands to. IPv4 and pure-hextet IPv6 are
// supported; embedded-IPv4 IPv6 (::ffff:1.2.3.4) is treated as unrecognised. ---

function ipv4ToInt(ip: string): number | null {
  const parts = ip.split(".");
  if (parts.length !== 4) return null;
  let n = 0;
  for (const p of parts) {
    if (!/^\d{1,3}$/.test(p)) return null;
    const v = Number(p);
    if (v > 255) return null;
    n = n * 256 + v;
  }
  return n >>> 0;
}
function intToIpv4(n: number): string {
  return [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255].join(".");
}
function ipv6ToBig(ip: string): bigint | null {
  if (ip.includes(".")) return null;
  const halves = ip.split("::");
  if (halves.length > 2) return null;
  let groups: string[];
  if (halves.length === 2) {
    const head = halves[0] ? halves[0].split(":") : [];
    const tail = halves[1] ? halves[1].split(":") : [];
    const fill = 8 - head.length - tail.length;
    if (fill < 1) return null;
    groups = [...head, ...Array(fill).fill("0"), ...tail];
  } else {
    groups = ip.split(":");
  }
  if (groups.length !== 8) return null;
  let n = 0n;
  for (const g of groups) {
    if (!/^[0-9a-fA-F]{1,4}$/.test(g)) return null;
    n = (n << 16n) + BigInt(parseInt(g, 16));
  }
  return n;
}
function bigToIpv6(n: bigint): string {
  const g: string[] = [];
  for (let i = 7; i >= 0; i--) g.push(((n >> BigInt(i * 16)) & 0xffffn).toString(16));
  // Compress the longest run (>= 2) of zero groups to "::".
  let bestStart = -1, bestLen = 0, curStart = -1, curLen = 0;
  for (let i = 0; i < 8; i++) {
    if (g[i] === "0") {
      if (curStart < 0) { curStart = i; curLen = 0; }
      if (++curLen > bestLen) { bestLen = curLen; bestStart = curStart; }
    } else { curStart = -1; curLen = 0; }
  }
  if (bestLen < 2) return g.join(":");
  return `${g.slice(0, bestStart).join(":")}::${g.slice(bestStart + bestLen).join(":")}`;
}

export type AclInfo =
  | { kind: "invalid" }
  | { kind: "single" }
  | { kind: "range"; first: string; last: string; count: bigint; exp: number };

// aclEntryInfo classifies one allow-list line.
export function aclEntryInfo(entry: string): AclInfo {
  const slash = entry.indexOf("/");
  if (slash < 0) {
    return ipv4ToInt(entry) !== null || ipv6ToBig(entry) !== null ? { kind: "single" } : { kind: "invalid" };
  }
  const addr = entry.slice(0, slash);
  const prefStr = entry.slice(slash + 1);
  if (!/^\d{1,3}$/.test(prefStr)) return { kind: "invalid" };
  const prefix = Number(prefStr);

  const v4 = ipv4ToInt(addr);
  if (v4 !== null) {
    if (prefix > 32) return { kind: "invalid" };
    const exp = 32 - prefix;
    const mask = prefix === 0 ? 0 : (0xffffffff << exp) >>> 0;
    const first = (v4 & mask) >>> 0;
    const count = 1n << BigInt(exp);
    const last = (first + Number(count) - 1) >>> 0;
    return { kind: "range", first: intToIpv4(first), last: intToIpv4(last), count, exp };
  }
  const v6 = ipv6ToBig(addr);
  if (v6 !== null) {
    if (prefix > 128) return { kind: "invalid" };
    const exp = 128 - prefix;
    const count = 1n << BigInt(exp);
    const first = prefix === 0 ? 0n : (v6 >> BigInt(exp)) << BigInt(exp);
    return { kind: "range", first: bigToIpv6(first), last: bigToIpv6(first + count - 1n), count, exp };
  }
  return { kind: "invalid" };
}

export const ACL_MAX_SAFE = 9_007_199_254_740_991n;
// fmtCount renders an address count, falling back to power-of-two form when it
// exceeds the safe-integer range (large IPv6 blocks).
export function fmtCount(count: bigint, exp: number): string {
  const noun = count === 1n ? "address" : "addresses";
  return count <= ACL_MAX_SAFE ? `${Number(count).toLocaleString()} ${noun}` : `2^${exp} addresses`;
}
