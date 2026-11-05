import { useEffect, useRef, useState, type ReactNode } from "react";
import { useRouterState } from "@tanstack/react-router";
import { Check, Copy, Link2 } from "lucide-react";
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

// CopyButton copies value to the clipboard. Every copy control in the console
// is this one icon, with no words: the copy glyph, swapped for a green check
// for a moment once copied. Its name stays for screen readers.
export function CopyButton({ value, className }: { value: string; className?: string }) {
  const { t } = useTranslation();
  const [copied, copy] = useCopy(value);
  const label = copied ? t("common.copied") : t("common.copy");
  return (
    <Button
      variant="ghost"
      size="icon-sm"
      type="button"
      aria-label={label}
      className={cn("shrink-0 self-center text-muted-foreground hover:text-foreground", className)}
      onClick={(event) => { event.stopPropagation(); copy(); }}
    >
      {copied
        ? <Check className="size-3.5 text-[var(--fx-success)]" aria-hidden="true" />
        : <Copy className="size-3.5" aria-hidden="true" />}
    </Button>
  );
}

// CopyIconButton is the same control for dense rows (a table cell, a key/value
// row): smaller, and dim until pointed at. It keeps its space whether or not it
// is hovered, so rows do not shift.
export function CopyIconButton({ value, className }: { value: string; className?: string }) {
  return (
    <CopyButton
      value={value}
      className={cn("size-6 opacity-60 transition-opacity hover:opacity-100 focus-visible:opacity-100", className)}
    />
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

// CopyLinkButton copies an absolute link to the current view, or to `href` (a
// path with its search and hash), so a teammate opens exactly what is on
// screen. Like every copy control it is an icon only, the link glyph.
export function CopyLinkButton({ href, className }: { href?: string; className?: string }) {
  const { t } = useTranslation();
  const current = useRouterState({ select: (state) => state.location.href });
  const [copied, copy] = useCopy(new URL(href ?? current, window.location.origin).toString());
  const label = copied ? t("common.link-copied") : t("common.copy-link");
  return (
    <Button variant="ghost" size="icon-sm" type="button" aria-label={label}
      className={cn("shrink-0 text-muted-foreground hover:text-foreground", className)}
      onClick={(event) => { event.stopPropagation(); copy(); }}>
      {copied
        ? <Check className="size-3.5 text-[var(--fx-success)]" aria-hidden="true" />
        : <Link2 className="size-3.5" aria-hidden="true" />}
    </Button>
  );
}
