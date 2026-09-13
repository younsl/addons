import { ReactNode, useEffect, useMemo, useState } from "react";
import { ChevronLeft, ChevronRight, ChevronsLeft, ChevronsRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { useTranslation } from "@/lib/i18n";

// highlightMatches wraps the parts of text matched by the active search in
// <mark>, so filtered rows show why they matched. re must carry the g flag.
export function highlightMatches(text: string | undefined, re: RegExp | null): ReactNode {
  if (!text || !re) return text;
  const parts: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(re)) {
    if (!m[0]) continue;
    const at = m.index ?? 0;
    if (at > last) parts.push(text.slice(last, at));
    parts.push(<mark key={at} className="rounded-[2px] bg-yellow-400/40 text-inherit">{m[0]}</mark>);
    last = at + m[0].length;
  }
  if (parts.length === 0) return text;
  if (last < text.length) parts.push(text.slice(last));
  return parts;
}

// parseQuery reads the vim/sed-style search convention: /pattern/ switches to
// regex matching, anything else is a plain substring. A lone "/" or "//" is
// treated as literal text.
function parseQuery(raw: string): { term: string; regex: boolean } {
  const m = raw.match(/^\/(.+)\/$/);
  return m ? { term: m[1], regex: true } : { term: raw, regex: false };
}

export interface TableSearch {
  query: string;
  setQuery: (q: string) => void;
  // True when the /pattern/ form is used and the pattern does not compile.
  regexError: boolean;
  // Active pattern with the g flag for highlightMatches; null when idle/invalid.
  highlightRe: RegExp | null;
  page: number;
  setPage: (p: number) => void;
  // Debounced, validated search params to send to the server. q is empty while
  // the pattern is invalid so the last good result set stays on screen.
  q: string;
  regex: boolean;
}

// useTableSearch owns the search box and page state for a table whose
// filtering and pagination run server-side. The query is debounced, then
// parsed: wrapping it in slashes (/pattern/) turns on regex matching, which is
// validated locally before it lands in q/regex for the caller's fetch effect.
export function useTableSearch(): TableSearch {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(0);
  const [debounced, setDebounced] = useState("");

  useEffect(() => {
    const id = setTimeout(() => setDebounced(query.trim()), 300);
    return () => clearTimeout(id);
  }, [query]);

  const { term, regex } = useMemo(() => parseQuery(debounced), [debounced]);

  const regexError = useMemo(() => {
    if (!regex) return false;
    try { new RegExp(term); return false; } catch { return true; }
  }, [term, regex]);

  const highlightRe = useMemo(() => {
    if (!term || regexError) return null;
    const source = regex ? term : term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return new RegExp(source, "gi");
  }, [term, regex, regexError]);

  return {
    query,
    setQuery: (q) => { setQuery(q); setPage(0); },
    regexError,
    highlightRe,
    page,
    setPage,
    q: regexError ? "" : term,
    regex,
  };
}

// TableSearchControls is the single search box; /pattern/ switches it to regex.
export function TableSearchControls({ search, className }: { search: TableSearch; className?: string }) {
  const { t } = useTranslation();
  return (
    <div className={cn("flex min-w-0 flex-1 items-center gap-2 max-sm:flex-col max-sm:items-stretch", className)}>
      <Input placeholder={t("common.search-all-columns")} aria-label={t("common.search-all-columns")}
        value={search.query} onChange={(e) => search.setQuery(e.target.value)} />
    </div>
  );
}

// TablePager renders Prev/Next plus the visible range. total is the number of
// rows matching the active search across all pages. Renders nothing while
// everything fits on one page.
export function TablePager({ page, pageSize, total, onPage, note }: {
  page: number;
  pageSize: number;
  total: number;
  onPage: (p: number) => void;
  // note sits beside the range, for what a table has to say about the rows
  // rather than about the page: how many are selected, most of all. It keeps the
  // row rendering even when there is only one page, since a selection spans
  // pages and its count has to survive them.
  note?: ReactNode;
}) {
  const { t } = useTranslation();
  const paged = total > pageSize;
  if (!paged && !note) return null;
  const pageCount = Math.max(1, Math.ceil(total / pageSize));
  const current = Math.min(page, pageCount - 1);
  const start = current * pageSize;
  return (
    <div className="mt-3 flex min-w-0 items-center gap-2 max-sm:flex-wrap max-sm:flex-col max-sm:items-stretch">
      {paged && (
        <>
          {/* Four moves, one shape each: to the ends and one step either way.
              The words live in the label and the title, so the row stays the
              same width in every language. */}
          <Button variant="outline" type="button" size="icon" disabled={current === 0}
            aria-label={t("common.first-page")} title={t("common.first-page")}
            onClick={() => onPage(0)}>
            <ChevronsLeft aria-hidden="true" />
          </Button>
          <Button variant="outline" type="button" size="icon" disabled={current === 0}
            aria-label={t("common.prev")} title={t("common.prev")}
            onClick={() => onPage(current - 1)}>
            <ChevronLeft aria-hidden="true" />
          </Button>
          <Button variant="outline" type="button" size="icon" disabled={current >= pageCount - 1}
            aria-label={t("common.next")} title={t("common.next")}
            onClick={() => onPage(current + 1)}>
            <ChevronRight aria-hidden="true" />
          </Button>
          <Button variant="outline" type="button" size="icon" disabled={current >= pageCount - 1}
            aria-label={t("common.last-page")} title={t("common.last-page")}
            onClick={() => onPage(pageCount - 1)}>
            <ChevronsRight aria-hidden="true" />
          </Button>
          {/* Which page, then which rows. The page number is what the buttons
              move and the range is what the screen shows; a reader jumping back
              later remembers the page, not the row numbers. */}
          <span className="text-sm text-muted-foreground">
            {t("common.page")} {(current + 1).toLocaleString()} / {pageCount.toLocaleString()}
          </span>
          <span className="text-sm text-muted-foreground">{start + 1}–{Math.min(start + pageSize, total)} of {total.toLocaleString()}</span>
        </>
      )}
      {note}
    </div>
  );
}
