import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { useTranslation } from "@/lib/i18n";

// useCopy holds the copy action and the short-lived "copied" acknowledgement
// shared by both button shapes. The timer is cleared on unmount so a component
// that disappears mid-acknowledgement (a table row re-rendering on refresh)
// never sets state afterwards.
function useCopy(value: string): [boolean, () => void] {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const copy = () => {
    navigator.clipboard?.writeText(value);
    setCopied(true);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 2000);
  };
  return [copied, copy];
}

// CopyButton copies value to the clipboard, flipping to "Copied" with a check
// mark for a moment as feedback.
export function CopyButton({ value }: { value: string }) {
  const { t } = useTranslation();
  const [copied, copy] = useCopy(value);
  return (
    <Button variant="outline" type="button" className="shrink-0" onClick={copy}>
      {copied
        ? <><Check className="size-3.5 text-[var(--fx-success)]" aria-hidden="true" /> {t("common.copied")}</>
        : t("common.copy")}
    </Button>
  );
}

// CopyIconButton is the same action reduced to an icon, for places where the
// value is the content and a labelled button would outweigh it - a table cell,
// a key/value row. It keeps its space whether or not it is hovered, so rows do
// not shift, and stays dim until pointed at.
export function CopyIconButton({ value, className }: { value: string; className?: string }) {
  const { t } = useTranslation();
  const [copied, copy] = useCopy(value);
  const label = copied ? t("common.copied") : t("common.copy");
  return (
    <Button
      variant="ghost"
      size="icon-xs"
      type="button"
      className={cn("shrink-0 text-muted-foreground opacity-60 transition-opacity hover:opacity-100 focus-visible:opacity-100", className)}
      title={label}
      onClick={copy}
    >
      {copied
        ? <Check className="text-[var(--fx-success)]" aria-hidden="true" />
        : <Copy aria-hidden="true" />}
      <span className="sr-only">{label}</span>
    </Button>
  );
}

// CopyOnHover puts the copy icon immediately right of the value it copies and
// keeps it out of sight until the pointer is over that value (or the button takes
// keyboard focus), fading in rather than snapping. It is for dense rows, where an
// always-visible icon per cell reads as a column of buttons instead of as an
// affordance on the value. The button keeps its box either way, so nothing shifts
// when it appears.
export function CopyOnHover({ value, children, className }: { value: string; children: ReactNode; className?: string }) {
  return (
    <span className={cn("group/copy inline-flex min-w-0 items-center gap-1", className)}>
      {children}
      <CopyIconButton
        value={value}
        className="opacity-0 duration-150 group-hover/copy:opacity-100 group-focus-within/copy:opacity-100"
      />
    </span>
  );
}
