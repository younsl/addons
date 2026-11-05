import { useMemo, useState, type ReactNode } from "react";

import { Button } from "@/components/ui/button";
import { SeverityBadge } from "@/components/app-ui/severity-badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useTranslation } from "@/lib/i18n";

import type { Advisory } from "@/components/app-ui/severity-bar";

type SortKey = "idx" | "id" | "severity" | "cvss";

const SEV_RANK: Record<string, number> = { critical: 4, high: 3, medium: 2, low: 1 };

// SortIcon is a stacked up/down chevron drawn inline as SVG (no icon library).
// When inactive both chevrons are muted to signal the column is sortable; when
// active the sorted direction is accented and the other dimmed.
function SortIcon({ state }: { state: "asc" | "desc" | null }) {
  const up = state === "asc" ? "var(--accent)" : state === "desc" ? "var(--border)" : "var(--muted)";
  const down = state === "desc" ? "var(--accent)" : state === "asc" ? "var(--border)" : "var(--muted)";

  return (
    <svg className="block shrink-0" width="11" height="14" viewBox="0 0 11 14" aria-hidden="true" focusable="false">
      <path d="M2 5 L5.5 1.5 L9 5" fill="none" stroke={up} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M2 9 L5.5 12.5 L9 9" fill="none" stroke={down} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

// AdvisoryTable renders the scan's advisories with every column sortable
// ascending/descending. # sorts by the original (as-scanned) order, severity by
// rank, and CVSS numerically.
export function AdvisoryTable({ advisories }: { advisories: Advisory[] }) {
  const { t } = useTranslation();
  const [key, setKey] = useState<SortKey>("idx");
  const [direction, setDirection] = useState<"asc" | "desc">("asc");

  // A missing score sorts below every real one rather than as zero, which would
  // put it alongside the genuinely harmless.
  const cvss = (advisory: Advisory) => {
    const score = parseFloat(advisory.score ?? "");
    return Number.isNaN(score) ? -1 : score;
  };

  const sorted = useMemo(() => {
    const rows = advisories.map((advisory, index) => ({ advisory, index }));

    rows.sort((left, right) => {
      let delta = 0;
      switch (key) {
        case "idx": delta = left.index - right.index; break;
        case "id": delta = left.advisory.id.localeCompare(right.advisory.id); break;
        case "severity":
          delta = (SEV_RANK[left.advisory.severity] ?? 0) - (SEV_RANK[right.advisory.severity] ?? 0);
          break;
        case "cvss": delta = cvss(left.advisory) - cvss(right.advisory); break;
      }
      return direction === "asc" ? delta : -delta;
    });

    return rows;
  }, [advisories, key, direction]);

  const onSort = (next: SortKey) => {
    if (next === key) setDirection(direction === "asc" ? "desc" : "asc");
    else { setKey(next); setDirection("asc"); }
  };

  const SortBtn = ({ k, children }: { k: SortKey; children: ReactNode }) => (
    <Button
      type="button"
      variant="ghost"
      size="xs"
      className="h-auto gap-1 p-0 text-xs uppercase text-inherit hover:bg-transparent hover:text-foreground"
      onClick={() => onSort(k)}
      aria-label={`Sort by ${k}`}
    >
      {children}
      <SortIcon state={key === k ? direction : null} />
    </Button>
  );

  return (
    <TableWrap className="mt-4">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className="w-14"><SortBtn k="idx">#</SortBtn></TableHead>
            <TableHead><SortBtn k="id">{t("common.advisory-id")}</SortBtn></TableHead>
            <TableHead><SortBtn k="severity">{t("common.severity")}</SortBtn></TableHead>
            <TableHead>
              <SortBtn k="cvss">CVSS</SortBtn>
              <Tooltip>
                <TooltipTrigger render={<span tabIndex={0} aria-label="CVSS help" />}>
                  <span className="ml-[5px] text-[0.85em] text-muted-foreground">ⓘ</span>
                </TooltipTrigger>
                <TooltipContent>{t("approval.cvss-help")}</TooltipContent>
              </Tooltip>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {sorted.map(({ advisory, index }) => (
            <TableRow key={advisory.id}>
              <TableCell className="text-muted-foreground">{index + 1}</TableCell>
              <TableCell className="font-mono text-xs">
                <a
                  className="underline underline-offset-4 hover:no-underline"
                  href={`https://osv.dev/${advisory.id}`}
                  target="_blank"
                  rel="noreferrer"
                >
                  {advisory.id}
                </a>
              </TableCell>
              <TableCell><SeverityBadge severity={advisory.severity} /></TableCell>
              <TableCell className="tabular-nums">
                {advisory.score || <span className="text-muted-foreground">{t("common.na")}</span>}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </TableWrap>
  );
}
