import * as React from "react"
import {
  flexRender,
  getCoreRowModel,
  getSortedRowModel,
  useReactTable,
  type ColumnDef,
  type SortingState,
} from "@tanstack/react-table"

import { useTranslation } from "@/lib/i18n"

import {
  Table as ShadcnTable,
  TableBody as ShadcnTableBody,
  TableCell as ShadcnTableCell,
  TableHead as ShadcnTableHead,
  TableHeader as ShadcnTableHeader,
  TableRow as ShadcnTableRow,
} from "@/components/ui/table"
import { cn } from "@/lib/utils"

function TableWrap({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="table-wrap"
      className={cn("w-full min-w-0 max-w-full overflow-x-auto rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel)] overscroll-x-contain [-webkit-overflow-scrolling:touch]", className)}
      {...props}
    />
  )
}

function Table({ className, ...props }: React.ComponentProps<"table">) {
  return (
    <ShadcnTable
      data-slot="table"
      className={cn("min-w-[720px] border-collapse text-[13px] max-sm:text-sm", className)}
      {...props}
    />
  )
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return <ShadcnTableHeader data-slot="table-header" className={className} {...props} />
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return <ShadcnTableBody data-slot="table-body" className={className} {...props} />
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <ShadcnTableRow
      data-slot="table-row"
      className={cn(
        "border-[var(--fx-border-subtle)] last:border-0 hover:bg-[var(--fx-surface-hover)] data-[state=selected]:bg-[var(--fx-surface-selected)]",
        className
      )}
      {...props}
    />
  )
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <ShadcnTableHead
      data-slot="table-head"
      className={cn(
        "h-auto border-b border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel-raised)] px-2 py-2 text-[11px] font-medium text-[var(--fx-text-subtle)] uppercase",
        className
      )}
      {...props}
    />
  )
}

function TableCell({ className, ...props }: React.ComponentProps<"td">) {
  return (
    <ShadcnTableCell
      data-slot="table-cell"
      className={cn("px-2 py-2 align-middle", className)}
      {...props}
    />
  )
}

// SortDir is a column sort direction; null on a SortIcon means "sortable but
// not the active column".
type SortDir = "asc" | "desc"

// SortIcon renders the ▲/▼ pair: both faint when inactive (signals sortable),
// the active direction highlighted otherwise.
function SortIcon({ state }: { state: SortDir | null }) {
  const up = state === "asc" ? "currentColor" : "var(--fx-text-faint, currentColor)"
  const down = state === "desc" ? "currentColor" : "var(--fx-text-faint, currentColor)"
  return (
    <svg className={cn("block shrink-0", state === null && "opacity-40")}
      width="11" height="14" viewBox="0 0 11 14" aria-hidden="true" focusable="false">
      <path d="M2 5 L5.5 1.5 L9 5" fill="none" stroke={up} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M2 9 L5.5 12.5 L9 9" fill="none" stroke={down} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

// cmpValues orders two cell values: numbers numerically, everything else as
// locale-aware numeric-friendly strings; null/undefined/empty always sort last.
function cmpValues(a: unknown, b: unknown): number {
  const emptyA = a === null || a === undefined || a === ""
  const emptyB = b === null || b === undefined || b === ""
  if (emptyA || emptyB) return emptyA === emptyB ? 0 : emptyA ? 1 : -1
  if (typeof a === "number" && typeof b === "number") return a - b
  if (typeof a === "boolean" && typeof b === "boolean") return Number(a) - Number(b)
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" })
}

// useSort adds client-side column sorting to a hand-rolled table. Give it the
// rows and one accessor per sortable column; it returns the sorted rows plus
// the state SortableHead consumes. Clicking a header toggles asc/desc; clicking
// another column starts it ascending. Sorting is stable (original order breaks
// ties) and empty values sink to the bottom in both directions.
function useSort<T>(rows: T[], accessors: Record<string, (row: T) => unknown>,
  initial?: { key: string; dir: SortDir }) {
  const [key, setKey] = React.useState<string | null>(initial?.key ?? null)
  const [dir, setDir] = React.useState<SortDir>(initial?.dir ?? "asc")
  const sorted = React.useMemo(() => {
    const acc = key ? accessors[key] : undefined
    if (!acc) return rows
    const mul = dir === "asc" ? 1 : -1
    return rows
      .map((r, i) => ({ r, i }))
      .sort((a, b) => cmpValues(acc(a.r), acc(b.r)) * mul || a.i - b.i)
      .map((x) => x.r)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows, key, dir])
  const onSort = (k: string) => {
    if (k === key) setDir((d) => (d === "asc" ? "desc" : "asc"))
    else { setKey(k); setDir("asc") }
  }
  return { sorted, sort: { key, dir, onSort } }
}

// SortState is the header-facing slice of useSort's return value.
type SortState = { key: string | null; dir: SortDir; onSort: (k: string) => void }

// SortableHead is a TableHead whose content is a sort toggle for one column.
function SortableHead({ k, sort, className, children }: {
  k: string
  sort: SortState
  className?: string
  children?: React.ReactNode
}) {
  const active = sort.key === k
  return (
    <TableHead className={className}
      aria-sort={active ? (sort.dir === "asc" ? "ascending" : "descending") : "none"}>
      <button type="button"
        className="inline-flex max-w-full items-center gap-1 uppercase text-inherit hover:text-foreground"
        onClick={() => sort.onSort(k)} aria-label={`Sort by ${k}`}>
        <span className="truncate">{children}</span>
        <SortIcon state={active ? sort.dir : null} />
      </button>
    </TableHead>
  )
}

function DataTable<TData>({
  columns,
  data,
  empty,
  className,
  tableClassName,
  rowTestId,
}: {
  columns: ColumnDef<TData>[]
  data: TData[]
  empty?: React.ReactNode
  className?: string
  tableClassName?: string
  // Names each row for browser tests, which cannot rely on row order.
  rowTestId?: (row: TData) => string
}) {
  const { t } = useTranslation()
  const [sorting, setSorting] = React.useState<SortingState>([])
  const table = useReactTable({
    data,
    columns,
    state: { sorting },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
  })
  const columnCount = table.getAllLeafColumns().length

  return (
    <TableWrap className={className}>
      <Table className={tableClassName}>
        <TableHeader>
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id}>
              {headerGroup.headers.map((header) => {
                const canSort = header.column.getCanSort()
                const dir = header.column.getIsSorted()
                return (
                  <TableHead key={header.id}
                    aria-sort={dir ? (dir === "asc" ? "ascending" : "descending") : canSort ? "none" : undefined}>
                    {header.isPlaceholder ? null : canSort ? (
                      <button type="button"
                        className="inline-flex max-w-full items-center gap-1 uppercase text-inherit hover:text-foreground"
                        onClick={header.column.getToggleSortingHandler()}>
                        <span className="truncate">
                          {flexRender(header.column.columnDef.header, header.getContext())}
                        </span>
                        <SortIcon state={dir === false ? null : dir} />
                      </button>
                    ) : (
                      flexRender(header.column.columnDef.header, header.getContext())
                    )}
                  </TableHead>
                )
              })}
            </TableRow>
          ))}
        </TableHeader>
        <TableBody>
          {table.getRowModel().rows.length > 0 ? (
            table.getRowModel().rows.map((row) => (
              <TableRow key={row.id} data-testid={rowTestId?.(row.original)}>
                {row.getVisibleCells().map((cell) => (
                  <TableCell key={cell.id}>
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </TableCell>
                ))}
              </TableRow>
            ))
          ) : (
            <TableRow>
              <TableCell colSpan={columnCount} className="text-muted-foreground">
                {empty ?? t("common.no-results")}
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </TableWrap>
  )
}

export {
  DataTable,
  SortableHead,
  SortIcon,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
  type ColumnDef,
  type SortState,
}
