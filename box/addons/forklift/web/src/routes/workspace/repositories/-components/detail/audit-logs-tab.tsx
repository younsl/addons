import type { ListRepositoryAuditLogsRequest } from "@/services/v1/repositories/types";
import { BrokenArtifact, useUserIds, userLink } from "@/routes/workspace/repositories/-components/detail/artifacts-tab";
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import type { Repository } from "@/services/v1/openapi-types";
import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { Select } from "@/components/app-ui/select";
import { SortableHead, Table, TableBody, TableCell, TableHeader, TableRow, TableWrap, useSort } from "@/components/app-ui/table";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useTranslation } from "@/lib/i18n";

// The document's own event enum. "" is this screen's addition, meaning "all",
// which the server expresses as the parameter being absent.
type AuditEvent = NonNullable<
  NonNullable<ListRepositoryAuditLogsRequest["query"]>["event"]
>;

const AUDIT_EVENTS = ["", "view", "download", "upload", "delete", "ttl.expire", "repo.create", "repo.update", "repo.delete",
  "approval.request", "approval.approve", "approval.reject", "artifact.label.add", "artifact.label.remove"];
const AUDIT_PAGE_SIZE = 50;

// auditTime renders an RFC3339 audit timestamp as "YYYY-MM-DD HH:MM:SS +00:00",
// keeping the timezone offset explicit (audit times are recorded in UTC, so a
// trailing "Z" is shown as "+00:00").
export function auditTime(iso: string): string {
  const m = iso?.match(/^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}:\d{2})(?:\.\d+)?(Z|[+-]\d{2}:\d{2})?/);
  if (!m) return iso ?? "";
  const tz = !m[3] || m[3] === "Z" ? "+00:00" : m[3];
  return `${m[1]} ${m[2]} ${tz}`;
}

// auditArtifactPath normalises a logged path to the artifact identity the server
// stores. Audit entries keep the raw request path, which for scoped npm packages
// is percent-encoded; a malformed escape is left as-is rather than throwing.
function auditArtifactPath(path: string): string {
  try {
    return decodeURIComponent(path);
  } catch {
    return path;
  }
}

export function AuditLogs({ repo }: { repo: Repository }) {
  const repoId = repo.id;
  const { t } = useTranslation();
  const { me } = useAuth();
  const userIds = useUserIds(Boolean(me.admin || me.auditor));
  // Audit rows name artifact paths, and some of those artifacts are unservable.
  // Marking them here saves a reader from correlating a logged 5xx against the
  // Artifacts tab by hand.
  //
  // The log stores the path as the client sent it, so a scoped npm package appears
  // percent-encoded (@scope%2Fname) while the registry keys on the decoded
  // identity. Lookups therefore go through auditArtifactPath.
  const [event, setEvent] = useState<AuditEvent | "">("");
  const [offset, setOffset] = useState(0);

  const logsQuery = useQuery({
    ...openApiQueryOptions.listRepositoryAuditLogs({
      path: { id: repoId },
      // An empty event means "all", which the server expresses as the parameter
      // being absent rather than as an empty string.
      query: { event: event || undefined, limit: AUDIT_PAGE_SIZE, offset },
    }),
    meta: { suppressGlobalErrorToast: true },
  });
  // Best-effort: a missing annotation must never keep the log from rendering,
  // so this query's failure is deliberately not surfaced.
  const danglingQuery = useQuery({
    ...openApiQueryOptions.getRepositoryDangling({ path: { id: repoId } }),
    meta: { suppressGlobalErrorToast: true },
  });

  const data = logsQuery.data;
  const error = getErrorMessageIfAny(logsQuery.error);
  const broken = new Map((danglingQuery.data ?? []).map((ref) => [ref.path, ref]));
  // Sorts within the loaded page; paging stays newest-first server-side.
  const { sorted, sort } = useSort(data?.logs ?? [], {
    time: (l) => l.created_at,
    event: (l) => l.event,
    path: (l) => l.path,
    user: (l) => l.username,
    status: (l) => l.status,
    ip: (l) => l.client_ip,
  });

  const refresh = () => logsQuery.refetch();

  return (
    <Card size="sm" className="mb-4">
      <CardContent>
      <div className="mb-4 flex items-start justify-between gap-3 max-sm:flex-col max-sm:items-stretch">
        <h2 className="m-0 text-base font-semibold">{t("repo.audit-log")}</h2>
        {data && <span className="text-sm text-muted-foreground">{data.count} events</span>}
      </div>
      <div className="flex min-w-0 items-center gap-2 mb-4 max-sm:flex-wrap items-stretch max-sm:flex-col">
        <Select value={event} onChange={(v) => { setEvent(v as AuditEvent | ""); setOffset(0); }}
          options={AUDIT_EVENTS.map((ev) => ({ value: ev, label: ev || "all events" }))} />
        <Button variant="outline" type="button" onClick={refresh}>{t("common.refresh")}</Button>
      </div>
      {error && <Alert className="mb-4">{error}</Alert>}
      <TableWrap>
      <Table>
        <TableHeader>
          <TableRow><SortableHead k="time" sort={sort}>{t("common.time")}</SortableHead><SortableHead k="event" sort={sort}>{t("common.event")}</SortableHead><SortableHead k="path" sort={sort}>{t("common.path")}</SortableHead><SortableHead k="user" sort={sort}>{t("common.user")}</SortableHead><SortableHead k="status" sort={sort}>{t("common.status")}</SortableHead><SortableHead k="ip" sort={sort}>{t("common.client-ip")}</SortableHead></TableRow>
        </TableHeader>
        <TableBody>
          {sorted.map((l) => (
            <TableRow key={l.id}>
              <TableCell className="text-muted-foreground whitespace-nowrap tabular-nums">{auditTime(l.created_at)}</TableCell>
              <TableCell><Badge>{l.event}</Badge></TableCell>
              <TableCell className="break-all font-mono text-xs">
                {l.path
                  ? (
                    <span className="flex items-start gap-1.5">
                      {broken.has(auditArtifactPath(l.path)) && (
                        <BrokenArtifact
                          repoType={repo.type}
                          since={broken.get(auditArtifactPath(l.path))?.first_seen}
                          lastSeen={broken.get(auditArtifactPath(l.path))?.last_seen}
                          lastStatus={broken.get(auditArtifactPath(l.path))?.last_status}
                        />
                      )}
                      <span>{l.path}</span>
                    </span>
                  )
                  : "-"}
              </TableCell>
              <TableCell className="truncate" title={l.username || t("common.anonymous")}>
                {l.username
                  ? userLink(l.username, userIds)
                  : <span className="text-muted-foreground">{t("common.anonymous")}</span>}
              </TableCell>
              <TableCell className={l.status >= 400 ? "text-destructive" : "text-muted-foreground"}>{l.status}</TableCell>
              <TableCell className="text-muted-foreground">{l.client_ip}</TableCell>
            </TableRow>
          ))}
          {data && data.logs.length === 0 && (
            <TableRow><TableCell colSpan={6} className="text-muted-foreground">{t("repo.no-audit-events")}</TableCell></TableRow>
          )}
        </TableBody>
      </Table>
      </TableWrap>
      {data && data.count > AUDIT_PAGE_SIZE && (
        <div className="flex min-w-0 items-center gap-2 mt-3 max-sm:flex-wrap max-sm:flex-col max-sm:items-stretch">
          <Button variant="outline" type="button" disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - AUDIT_PAGE_SIZE))}>{t("common.newer")}</Button>
          <Button variant="outline" type="button" disabled={offset + AUDIT_PAGE_SIZE >= data.count}
            onClick={() => setOffset(offset + AUDIT_PAGE_SIZE)}>{t("common.older")}</Button>
          <span className="text-sm text-muted-foreground">{offset + 1}–{Math.min(offset + AUDIT_PAGE_SIZE, data.count)} of {data.count}</span>
        </div>
      )}
      </CardContent>
    </Card>
  );
}
