import { formatFileSize } from "@/utils/format-file-size";
import type React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Boxes, ClipboardCheck, FileArchive, Search, UserRound, UsersRound } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { highlightMatches } from "@/components/app-ui/table-search";
import { Badge } from "@/components/ui/badge";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

// One selectable row in the results list, already resolved to a destination.
type Hit = {
  key: string;
  group: string;
  // Exact total matches in this hit's section (may exceed the listed items).
  groupCount: number;
  icon: React.ComponentType<{ className?: string }>;
  title: string;
  sub?: string;
  badge?: string;
  go: () => void;
};

// GlobalSearch is the sidebar's unified search: a trigger styled like an input
// plus a command-palette dialog (also on Cmd/Ctrl+K). Results come from
// GET /search, which the server already narrows to what the caller may see, so
// the UI simply hides sections that come back null.
export function GlobalSearch() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const [active, setActive] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    if (!open) {
      setQ("");
      setActive(0);
    }
  }, [open]);

  const term = q.trim();
  const debouncedTerm = useDebouncedValue(term, 200);
  // Two characters is the shortest term worth a round trip; one would match
  // most of the instance and rank meaninglessly.
  const searchQuery = useQuery({
    ...openApiQueryOptions.getSearch({ query: { q: debouncedTerm, limit: 5 } }),
    enabled: open && debouncedTerm.length >= 2,
    // A search result is only as good as the moment it was asked for, but
    // repeating an identical term within the dialog session can reuse it.
    staleTime: 10_000,
    meta: { suppressGlobalErrorToast: true },
  });
  // Held while the next term is in flight rather than cleared, so the list does
  // not blank out between keystrokes. Below the minimum length there is nothing
  // to show at all.
  const result = term.length >= 2 ? (searchQuery.data ?? null) : null;

  // The highlighted row resets whenever the result set changes underneath it,
  // so a keyboard selection cannot end up pointing at a row that has moved.
  useEffect(() => { setActive(0); }, [result]);

  const hits = useMemo<Hit[]>(() => {
    if (!result) return [];
    const go = (to: string, id: number) => () => {
      setOpen(false);
      navigate({ to, params: { id: String(id) } });
    };
    const out: Hit[] = [];
    const count = (key: string, listed: number) => result.counts?.[key] ?? listed;
    // Every field of every hit is optional in the generated types: SearchResult
    // declares its five inline item shapes with no `required` at all, though
    // the server always populates them. Rather than assert otherwise, the
    // fallbacks below make a partial hit render as a partial row.
    for (const r of result.repositories ?? []) {
      out.push({
        key: `repo-${r.id}`, group: t("common.repositories"), icon: Boxes,
        groupCount: count("repositories", result.repositories?.length ?? 0),
        title: r.name ?? "", sub: `${r.format} · ${r.type}`,
        go: go("/workspace/repositories/$id", r.id ?? 0),
      });
    }
    for (const a of result.artifacts ?? []) {
      out.push({
        key: `art-${a.repo_id}-${a.path}`, group: t("common.artifacts"), icon: FileArchive,
        groupCount: count("artifacts", result.artifacts?.length ?? 0),
        title: a.path ?? "", sub: `${a.repo_name} · ${formatFileSize(a.size ?? 0)}`,
        go: go("/workspace/repositories/$id", a.repo_id ?? 0),
      });
    }
    for (const a of result.approvals ?? []) {
      out.push({
        key: `appr-${a.id}`, group: t("nav.approvals"), icon: ClipboardCheck,
        groupCount: count("approvals", result.approvals?.length ?? 0),
        title: a.package ?? "", sub: a.repo_name, badge: a.status,
        go: go("/workspace/approvals/$id", a.id ?? 0),
      });
    }
    for (const u of result.users ?? []) {
      out.push({
        key: `user-${u.id}`, group: t("nav.users"), icon: UserRound,
        groupCount: count("users", result.users?.length ?? 0),
        title: u.username ?? "", sub: u.robot ? "robot" : undefined,
        go: go("/access/users/$id", u.id ?? 0),
      });
    }
    for (const r of result.roles ?? []) {
      out.push({
        key: `role-${r.id}`, group: t("nav.roles"), icon: UsersRound,
        groupCount: count("roles", result.roles?.length ?? 0),
        title: r.name ?? "", sub: r.description || undefined,
        go: go("/access/roles/$id", r.id ?? 0),
      });
    }
    return out;
  }, [result, navigate, t]);

  // Wraps the matched part of each hit in <mark>, mirroring the table search.
  const highlightRe = useMemo(() => {
    const term = q.trim();
    if (term.length < 2) return null;
    return new RegExp(term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi");
  }, [q]);

  const onInputKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => Math.min(i + 1, hits.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter" && hits[active]) {
      e.preventDefault();
      hits[active].go();
    }
  };

  const showEmpty = result !== null && hits.length === 0;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="mb-2 flex h-9 w-full items-center gap-2 rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-1)] px-2 text-[13px] text-[var(--fx-text-subtle)] transition-colors hover:bg-[var(--fx-surface-hover)] hover:text-foreground max-lg:hidden"
      >
        <Search className="size-3.5 shrink-0" aria-hidden="true" />
        <span>{t("common.search")}</span>
        <kbd className="ml-auto rounded border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel)] px-1 font-sans text-[10px] leading-4">⌘K</kbd>
      </button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent
          showCloseButton={false}
          className="top-[18%] translate-y-0 gap-0 overflow-hidden p-0 sm:max-w-xl"
          initialFocus={inputRef}
        >
          <DialogTitle className="sr-only">{t("common.search")}</DialogTitle>
          <div className="flex items-center gap-2 border-b border-[var(--fx-border-subtle)] px-3">
            <Search className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
            <input
              ref={inputRef}
              value={q}
              onChange={(e) => setQ(e.target.value)}
              onKeyDown={onInputKeyDown}
              placeholder={t("search.placeholder")}
              aria-label={t("common.search")}
              className="h-11 w-full bg-transparent text-sm outline-none placeholder:text-muted-foreground"
            />
          </div>
          <div className="max-h-[50dvh] overflow-y-auto p-2">
            {q.trim().length < 2 && (
              <div className="px-2 py-6 text-center text-sm text-muted-foreground">{t("search.type-to-search")}</div>
            )}
            {showEmpty && (
              <div className="px-2 py-6 text-center text-sm text-muted-foreground">{t("common.no-results")}</div>
            )}
            {hits.map((h, i) => (
              <div key={h.key}>
                {(i === 0 || hits[i - 1].group !== h.group) && (
                  <div className="flex items-center gap-1.5 px-2 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wide text-[var(--fx-text-subtle)]">
                    <span>{h.group}</span>
                    <Badge variant="outline" className="min-w-4 justify-center px-1 text-[10px] leading-4">
                      {h.groupCount}
                    </Badge>
                  </div>
                )}
                <button
                  type="button"
                  onClick={h.go}
                  onMouseEnter={() => setActive(i)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm",
                    i === active ? "bg-[var(--fx-surface-selected)] text-foreground" : "text-muted-foreground",
                  )}
                >
                  <h.icon className="size-4 shrink-0 opacity-75" aria-hidden="true" />
                  <span className="min-w-0 flex-1 truncate text-foreground">{highlightMatches(h.title, highlightRe)}</span>
                  {h.sub && <span className="max-w-[40%] truncate text-xs text-muted-foreground">{highlightMatches(h.sub, highlightRe)}</span>}
                  {h.badge && <Badge variant="outline">{h.badge}</Badge>}
                </button>
              </div>
            ))}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

// The dialog fires a request per pause in typing, not per keystroke.
function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);

  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(timer);
  }, [value, delayMs]);

  return debounced;
}
