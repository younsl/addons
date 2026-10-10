import type { MessageKey } from "@/lib/i18n";

// Why a project is out of scope, as the scanner records it: "muted" when an
// operator silenced it from the console, or "topic:<name>" when the repository
// opted itself out. Neither is a string to put in front of a reader, so both
// resolve to copy here rather than at each call site.
export interface ExclusionReason {
  key: MessageKey;
  // topic is the GitLab topic that matched, for the topic case only.
  topic?: string;
}

export function readExclusionReason(reason: string): ExclusionReason | null {
  if (!reason) return null;
  if (reason === "muted") return { key: "coverage.muted-by-console" };
  if (reason.startsWith("topic:")) {
    return { key: "coverage.muted-by-topic", topic: reason.slice("topic:".length) };
  }
  return { key: "coverage.excluded" };
}

// The checks an operator can mute one at a time. A project with every one muted
// is out of the measurement entirely, which is what a bare mute used to mean.
export type MuteScope = "ci" | "registry";

export const MUTE_SCOPES: MuteScope[] = ["ci", "registry"];

export const muteScopeLabelKey: Record<MuteScope, MessageKey> = {
  ci: "coverage.ci",
  registry: "coverage.registry",
};

// What each check actually looks at. It travels with the label everywhere the
// two halves are shown, because "CI" and "registry" are only obvious to whoever
// already knows what the scan reads.
export const muteScopeHintKey: Record<MuteScope, MessageKey> = {
  ci: "coverage.ci-hint",
  registry: "coverage.registry-hint",
};

// toggleMuteScope returns the whole list the API expects, not a delta: what it
// leaves out is what gets unmuted. The order is fixed so the same selection is
// always the same request.
export function toggleMuteScope(current: readonly string[], scope: MuteScope, on: boolean): MuteScope[] {
  const next = new Set(current.filter((s): s is MuteScope => MUTE_SCOPES.includes(s as MuteScope)));
  if (on) next.add(scope);
  else next.delete(scope);
  return MUTE_SCOPES.filter((s) => next.has(s));
}

// isMuted reports whether one check is waived on a project.
export function isMuted(scopes: readonly string[] | undefined, scope: MuteScope): boolean {
  return !!scopes?.includes(scope);
}
