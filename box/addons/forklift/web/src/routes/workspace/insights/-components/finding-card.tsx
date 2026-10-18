import { Link } from "@tanstack/react-router";
import { BadgeCheck, ChevronRight, CircleAlert, CircleCheck, CircleMinus, TriangleAlert } from "lucide-react";
import type { ReactNode } from "react";

import { Badge } from "@/components/app-ui/badge";
import { SeverityBadge } from "@/components/app-ui/severity-badge";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { InsightAffected, InsightCheck, InsightFinding } from "@/services/v1/openapi-types";

export type CheckStatus = InsightCheck["status"];

// The repository tab a repository row opens: policy findings land where the
// policy is edited, the scan gap on the numbers behind it.
const REPO_TAB: Record<string, string> = {
  "vuln-policy-off": "security",
  "age-policy-off": "security",
  "license-policy-off": "security",
  "scan-gap": "statistics",
  "vuln-accepted": "security",
};

// useInsightText renders a rule's text in the console language, filling the
// finding's figures in. A finding narrowed to some repositories keeps only its
// count, so a summary that needs more falls back to the scoped sentence; a rule
// the console has no text for keeps the server's English.
export function useInsightText() {
  const { t } = useTranslation();
  const lookup = (key: string) => t(key as MessageKey) as string | undefined;
  const fill = (template: string, params: Record<string, string>) =>
    template.replace(/\{(\w+)\}/g, (whole, name: string) => params[name] ?? whole);
  return {
    title: (r: { id: string; title: string }) => lookup(`insight.${r.id}.title`) ?? r.title,
    why: (r: { id: string; rationale: string }) => lookup(`insight.${r.id}.why`) ?? r.rationale,
    criteria: (r: { id: string; criteria: string }) => lookup(`insight.${r.id}.criteria`) ?? r.criteria,
    action: (r: { id: string; recommendation: string }) => lookup(`insight.${r.id}.action`) ?? r.recommendation,
    // English has a singular form per rule (`summary-one`); a language without
    // one, such as Korean, falls through to the regular sentence.
    summary: (f: InsightFinding) => {
      const single = f.total === 1;
      const template = (single && lookup(`insight.${f.id}.summary-one`)) || lookup(`insight.${f.id}.summary`);
      if (!template) return f.summary;
      const filled = fill(template, f.params);
      if (!/\{\w+\}/.test(filled)) return filled;
      const scoped = (single && lookup("insights.summary-scoped-one")) || t("insights.summary-scoped");
      return fill(scoped, f.params);
    },
  };
}

const STATUS_TONE: Record<CheckStatus, string> = {
  action: "text-[var(--fx-danger)]",
  investigate: "text-[var(--fx-warning)]",
  acknowledged: "text-[var(--fx-info)]",
  ok: "text-[var(--fx-success)]",
  unavailable: "text-muted-foreground",
};

// StatusIcon is the Trusted Advisor style signal: red for action recommended,
// yellow for investigation recommended, green for no problems detected.
export function StatusIcon({ status, className }: { status: CheckStatus; className?: string }) {
  const Icon = status === "action" ? CircleAlert
    : status === "investigate" ? TriangleAlert
      : status === "acknowledged" ? BadgeCheck
        : status === "ok" ? CircleCheck : CircleMinus;
  return <Icon className={cn("size-4 shrink-0", STATUS_TONE[status], className)} aria-hidden="true" />;
}

export function StatusLabel({ status }: { status: CheckStatus }) {
  const { t } = useTranslation();
  return (
    <span className={cn("inline-flex items-center gap-1.5 whitespace-nowrap text-sm font-medium", STATUS_TONE[status])}>
      <StatusIcon status={status} />
      {t(`insights.status-${status}` as MessageKey)}
    </span>
  );
}

export function InsightSeverityBadge({ severity }: { severity: InsightFinding["severity"] }) {
  const { t } = useTranslation();
  return (
    <SeverityBadge severity={severity}
      className={severity === "info" ? "border-border bg-transparent text-muted-foreground" : undefined}>
      {t(`insights.severity-${severity}` as MessageKey)}
    </SeverityBadge>
  );
}

export function AffectedLink({ finding, item }: { finding: InsightFinding; item: InsightAffected }): ReactNode {
  const id = item.repo_id != null ? String(item.repo_id) : undefined;
  switch (item.kind) {
    case "artifact":
      return id ? (
        <Link to="/workspace/repositories/$id/$tab" params={{ id, tab: "artifacts" }} search={{ q: item.path }}>
          {item.label}
        </Link>
      ) : item.label;
    case "tag":
      return id ? (
        <Link to="/workspace/repositories/$id/oci-artifact" params={{ id }} search={{ name: item.path ?? "", ref: item.tag ?? "" }}>
          {item.label}
        </Link>
      ) : item.label;
    case "repository":
      return id ? (
        <Link to="/workspace/repositories/$id/$tab" params={{ id, tab: REPO_TAB[finding.id] ?? "settings" }}>
          {item.label}
        </Link>
      ) : item.label;
    case "user":
    case "token":
      return item.user_id != null ? (
        <Link to="/access/users/$id" params={{ id: String(item.user_id) }}>{item.label}</Link>
      ) : item.label;
    case "approval":
      return <Link to="/workspace/approvals">{item.label}</Link>;
    case "advisory":
      return (
        <a href={`https://osv.dev/vulnerability/${encodeURIComponent(item.label)}`} target="_blank" rel="noreferrer">
          {item.label}
        </a>
      );
    case "project":
      return <Link to="/workspace/coverage/project" search={{ path: item.label }}>{item.label}</Link>;
    case "volume":
      return <Link to="/admin/storage">{item.label}</Link>;
    default:
      return item.label;
  }
}

// CheckRow is one rule in the list. The whole row links to its detail page,
// which carries the reason, the alert criteria, the action and the items. With
// `locked` (a viewer limited to some repositories) the row keeps its title and
// status but is dimmed and inert: the detail pages are for administrators,
// auditors and security reviewers.
export function CheckRow({ check, finding, showCategory = true, locked = false }: {
  check: InsightCheck;
  finding?: InsightFinding;
  showCategory?: boolean;
  locked?: boolean;
}) {
  const { t } = useTranslation();
  const text = useInsightText();
  const body = (
    <>
      <StatusIcon status={check.status} className="mt-0.5" />
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 flex-wrap items-center gap-2">
          <CheckCode code={check.code} />
          <span className="font-medium text-foreground">{text.title(check)}</span>
          {finding && <InsightSeverityBadge severity={finding.severity} />}
          {check.flagged > 0 && (
            <span className="text-sm tabular-nums text-muted-foreground">
              {t("insights.flagged").replace("{count}", check.flagged.toLocaleString())}
            </span>
          )}
        </span>
        <span className="mt-1 block text-sm leading-relaxed text-muted-foreground">
          {finding ? text.summary(finding) : text.why(check)}
        </span>
        <AckLine check={check} />
        <span className="mt-2 flex flex-wrap gap-1.5">
          {showCategory && <Badge variant="outline">{t(`insights.category-${check.category}` as MessageKey)}</Badge>}
          {check.audiences.map((a) => (
            <Badge key={a} variant="secondary">{t(`insights.audience-${a}` as MessageKey)}</Badge>
          ))}
        </span>
      </span>
    </>
  );
  if (locked) {
    return (
      <div aria-disabled="true"
        className="flex min-w-0 cursor-not-allowed select-none items-start gap-3 border-b border-border px-4 py-3 opacity-50 last:border-b-0">
        {body}
      </div>
    );
  }
  return (
    <Link to="/workspace/insights/$id" params={{ id: check.id }}
      className="group flex min-w-0 items-start gap-3 border-b border-border px-4 py-3 last:border-b-0 hover:bg-[var(--fx-surface-hover)] hover:no-underline">
      {body}
      <ChevronRight className="mt-0.5 size-4 shrink-0 text-muted-foreground group-hover:text-foreground" aria-hidden="true" />
    </Link>
  );
}

// AckLine states an acknowledgement in one line: who and until when while it
// holds, or why it lapsed so the reader knows the check counts again.
export function AckLine({ check }: { check: InsightCheck }) {
  const { t } = useTranslation();
  const ack = check.acknowledgement;
  if (!ack) return null;
  const until = ack.expires_at.slice(0, 10);
  if (ack.state === "active") {
    return (
      <span className="mt-1 block text-xs text-[var(--fx-info)]">
        {t("insights.ack-line").replace("{user}", ack.acknowledged_by).replace("{date}", until)}
      </span>
    );
  }
  return (
    <span className="mt-1 block text-xs text-[var(--fx-warning)]">
      {ack.state === "expired"
        ? t("insights.ack-expired").replace("{date}", until)
        : t("insights.ack-new-items").replace("{count}", String(check.flagged - ack.flagged))}
    </span>
  );
}

// CheckCode is a check's stable short code (AC-05), the name to use in a
// ticket, a chat or an alarm.
export function CheckCode({ code }: { code: string }) {
  if (!code) return null;
  return (
    <span className="rounded border border-border px-1.5 font-mono text-xs leading-5 text-muted-foreground">{code}</span>
  );
}
