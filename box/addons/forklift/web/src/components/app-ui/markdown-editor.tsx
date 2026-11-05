import { useRef, type ReactNode } from "react";

import { cn } from "@/lib/utils";

// Token colours, from the console's own tokens so the editor follows the light
// and dark themes. Only colour changes, never weight or size, so the coloured
// layer keeps every character exactly under the caret in the text area.
const TONE = {
  marker: "text-[var(--fx-accent-ink)]",
  heading: "text-foreground",
  code: "text-[var(--fx-success)]",
  url: "text-[var(--fx-info)]",
  bracket: "text-muted-foreground",
  label: "text-foreground",
  list: "text-[var(--fx-warning)]",
  emoji: "text-[var(--fx-warning)]",
  quote: "text-muted-foreground",
} as const;

// Inline Markdown: `code`, [label](url), **bold** or __bold__, *italic* or
// _italic_, :emoji: shortcodes and bare links. Text between them is kept as
// written.
const INLINE = String.raw`(\x60[^\x60\n]+\x60)|\[([^\]\n]+)\]\(([^)\s]+)\)|(\*\*|__)([^\n]+?)\4|(\*|_)([^\s*_][^\n]*?)\6|(:[a-z0-9_+-]+:)|(https?:\/\/[^\s<>()]+)`;

function inline(text: string, key: string): ReactNode[] {
  const out: ReactNode[] = [];
  // A fresh expression per call: emphasis content is highlighted recursively,
  // and a shared global regex would have its position reset under the outer loop.
  const re = new RegExp(INLINE, "g");
  let last = 0;
  let k = 0;
  let m: RegExpExecArray | null;
  const span = (tone: string, value: ReactNode) => <span key={`${key}-${k++}`} className={tone}>{value}</span>;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push(text.slice(last, m.index));
    if (m[1]) out.push(span(TONE.code, m[1]));
    else if (m[2] !== undefined) {
      out.push(span(TONE.bracket, "["), span(TONE.label, m[2]), span(TONE.bracket, "]("), span(TONE.url, m[3]), span(TONE.bracket, ")"));
    } else if (m[4]) {
      out.push(span(TONE.marker, m[4]), span(TONE.label, inline(m[5], `${key}-${k++}`)), span(TONE.marker, m[4]));
    } else if (m[6]) {
      out.push(span(TONE.marker, m[6]), span(TONE.label, inline(m[7], `${key}-${k++}`)), span(TONE.marker, m[6]));
    } else if (m[8]) out.push(span(TONE.emoji, m[8]));
    else if (m[9]) out.push(span(TONE.url, m[9]));
    last = re.lastIndex;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

// highlightMarkdown colours Markdown source line by line: headings, quotes,
// list markers and fenced code blocks, then the inline syntax within a line.
export function highlightMarkdown(source: string): ReactNode[] {
  let fenced = false;
  return source.split("\n").map((line, i) => {
    const key = String(i);
    let body: ReactNode;
    if (/^\s*```/.test(line)) {
      fenced = !fenced;
      body = <span className={TONE.code}>{line}</span>;
    } else if (fenced) {
      body = <span className={TONE.code}>{line}</span>;
    } else {
      const heading = /^(#{1,6}\s)(.*)$/.exec(line);
      const quote = /^(\s*>\s?)(.*)$/.exec(line);
      const list = /^(\s*)([-*+]|\d+\.)(\s)(.*)$/.exec(line);
      if (heading) {
        body = <><span className={TONE.marker}>{heading[1]}</span><span className={TONE.heading}>{inline(heading[2], key)}</span></>;
      } else if (quote) {
        body = <><span className={TONE.quote}>{quote[1]}</span>{inline(quote[2], key)}</>;
      } else if (list) {
        body = <>{list[1]}<span className={TONE.list}>{list[2]}</span>{list[3]}{inline(list[4], key)}</>;
      } else {
        body = inline(line, key);
      }
    }
    return <span key={key}>{body}{"\n"}</span>;
  });
}

// MarkdownEditor is a text area with Markdown syntax colouring. The text area
// stays the real input (caret, selection, undo, paste); its text is made
// transparent over a coloured copy of the same source, scrolled in step.
export function MarkdownEditor({ value, onChange, placeholder, maxLength, className }: {
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  maxLength?: number;
  className?: string;
}) {
  const layer = useRef<HTMLPreElement>(null);
  const shared = "m-0 px-3 py-2 font-mono text-sm leading-6 whitespace-pre-wrap break-all [scrollbar-gutter:stable]";
  return (
    <div className={cn("relative h-[280px] overflow-hidden rounded-sm border border-input dark:bg-input/30 focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50", className)}>
      <pre ref={layer} aria-hidden="true" className={cn(shared, "pointer-events-none absolute inset-0 overflow-hidden text-muted-foreground")}>
        {highlightMarkdown(value)}
        {/* Keeps the layer as tall as the text area when the source ends in a newline. */}
        {" "}
      </pre>
      <textarea
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onScroll={(e) => {
          if (layer.current) layer.current.scrollTop = e.currentTarget.scrollTop;
        }}
        placeholder={placeholder}
        maxLength={maxLength}
        spellCheck={false}
        className={cn(shared, "absolute inset-0 size-full resize-none overflow-auto bg-transparent text-transparent caret-foreground outline-none placeholder:text-muted-foreground selection:bg-[var(--fx-info)]/30")}
      />
    </div>
  );
}
