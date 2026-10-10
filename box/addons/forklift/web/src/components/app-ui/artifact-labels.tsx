import { useEffect, useRef, useState } from "react";
import { Plus, X } from "lucide-react";

import { api, type ArtifactLabel } from "@/api";
import { Badge } from "@/components/app-ui/badge";
import { highlightMatches } from "@/components/app-ui/table-search";
import { Input } from "@/components/ui/input";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

// Mirrors the server's rule (src/meta/label.rs): a bare key or a key:value
// pair, each side ASCII letters, digits, '-' and '_'. Checked here only to keep
// an obviously invalid value from making a round trip; the server is the
// authority.
const LABEL_RE = /^[A-Za-z0-9_-]+(:[A-Za-z0-9_-]+)?$/;

type Props = {
  repoId: number;
  // Stored artifact path, which is the label identity for every format.
  path: string;
  labels: ArtifactLabel[];
  // Whether this viewer may change the labels: administrator on the repository,
  // or the principal who uploaded this artifact. Decided per artifact by the
  // server, so the control simply follows it.
  canLabel: boolean;
  highlightRe?: RegExp | null;
  // Reports a failed mutation to the surrounding view, which already owns an
  // error banner; the control keeps the typed value so it can be retried.
  onError?: (message: string) => void;
  className?: string;
};

// ArtifactLabels shows one artifact's labels and, for a viewer allowed to change
// them, the add and remove controls. Both mutations answer with the labels as the
// server now holds them, and the list is replaced by that answer rather than
// predicted locally, so a rejected or normalised value is never shown as applied.
export function ArtifactLabels({ repoId, path, labels, canLabel, highlightRe, onError, className }: Props) {
  const { t } = useTranslation();
  const [current, setCurrent] = useState(labels);
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // The parent reloads the listing after its own mutations (a delete, a page
  // change), so the prop is the source of truth whenever it changes.
  useEffect(() => setCurrent(labels), [labels]);
  useEffect(() => {
    if (adding) inputRef.current?.focus();
  }, [adding]);

  const fail = (e: unknown) => onError?.((e as Error).message);

  const add = async () => {
    const value = draft.trim();
    if (!value) {
      setAdding(false);
      return;
    }
    if (value.length > 64 || !LABEL_RE.test(value)) {
      onError?.(t("label.invalid"));
      return;
    }
    setBusy(true);
    try {
      const updated = await api.addArtifactLabel(repoId, path, value);
      setCurrent(updated.labels);
      setDraft("");
      setAdding(false);
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (label: string) => {
    setBusy(true);
    try {
      const updated = await api.removeArtifactLabel(repoId, path, label);
      setCurrent(updated.labels);
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={cn("flex min-w-0 flex-wrap items-center gap-1.5", className)}>
      {current.map((l) => (
        <Badge
          key={l.label}
          variant="secondary"
          title={l.created_by ? `${l.label}, ${t("label.added-by")} ${l.created_by}` : l.label}
        >
          <span className="max-w-48 truncate">{highlightMatches(l.label, highlightRe ?? null)}</span>
          {canLabel && (
            <button
              type="button"
              disabled={busy}
              onClick={() => remove(l.label)}
              aria-label={`${t("label.remove")}: ${l.label}`}
              title={t("label.remove")}
              className="ml-1 inline-flex items-center text-muted-foreground hover:text-foreground disabled:opacity-50"
            >
              <X className="size-3" aria-hidden="true" />
            </button>
          )}
        </Badge>
      ))}
      {adding ? (
        <Input
          ref={inputRef}
          value={draft}
          disabled={busy}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              add();
            } else if (e.key === "Escape") {
              e.preventDefault();
              setDraft("");
              setAdding(false);
            }
          }}
          onBlur={() => {
            if (!draft.trim()) setAdding(false);
          }}
          placeholder={t("label.placeholder")}
          aria-label={t("label.add")}
          className="h-6 w-44 px-1.5 text-xs"
        />
      ) : (
        canLabel && (
          <button
            type="button"
            onClick={() => setAdding(true)}
            title={t("label.add")}
            aria-label={t("label.add")}
            className="inline-flex items-center gap-1 rounded-md border border-dashed border-[var(--fx-border-subtle)] px-1.5 py-0.5 text-xs text-muted-foreground hover:border-border hover:text-foreground"
          >
            <Plus className="size-3" aria-hidden="true" />
            {current.length === 0 && <span>{t("label.add")}</span>}
          </button>
        )
      )}
      {current.length === 0 && !canLabel && !adding && <span className="text-muted-foreground">-</span>}
    </div>
  );
}
