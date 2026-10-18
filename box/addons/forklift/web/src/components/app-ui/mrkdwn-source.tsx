import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

// Token colours for Slack/Mattermost mrkdwn source, from the console's own
// tokens so the block follows the light and dark themes.
const TONE = {
  marker: "text-[var(--fx-accent-ink)]",
  url: "text-[var(--fx-info)]",
  bracket: "text-muted-foreground",
  label: "text-foreground",
  number: "text-[var(--fx-warning)]",
} as const;

// One pass over a line: <url|label> links, *bold* and _italic_ spans. Text
// between them stays as written; only colour is added, never a character.
const TOKEN_SOURCE = String.raw`<([^|>]+)\|([^>]+)>|\*([^*\n]+)\*|_([^_\n]+)_`;

function highlightLine(line: string, key: string): ReactNode[] {
  const out: ReactNode[] = [];
  const list = /^(\d+\.)(\s)/.exec(line);
  let rest = line;
  let k = 0;
  if (list) {
    out.push(<span key={`${key}-${k++}`} className={TONE.number}>{list[1]}</span>, list[2]);
    rest = line.slice(list[0].length);
  }
  // A fresh expression per call: bold content is highlighted recursively, and
  // a shared global regex would have its position reset under the outer loop.
  const token = new RegExp(TOKEN_SOURCE, "g");
  let last = 0;
  for (let m = token.exec(rest); m; m = token.exec(rest)) {
    if (m.index > last) out.push(rest.slice(last, m.index));
    if (m[1] !== undefined) {
      out.push(
        <span key={`${key}-${k++}`}>
          <span className={TONE.bracket}>&lt;</span>
          <span className={TONE.url}>{m[1]}</span>
          <span className={TONE.bracket}>|</span>
          <span className={TONE.label}>{m[2]}</span>
          <span className={TONE.bracket}>&gt;</span>
        </span>,
      );
    } else {
      const marker = m[3] !== undefined ? "*" : "_";
      const inner = m[3] ?? m[4] ?? "";
      out.push(
        <span key={`${key}-${k++}`}>
          <span className={TONE.marker}>{marker}</span>
          <span className={cn(TONE.label, marker === "*" ? "font-semibold" : "italic")}>{highlightLine(inner, `${key}-${k++}`)}</span>
          <span className={TONE.marker}>{marker}</span>
        </span>,
      );
    }
    last = token.lastIndex;
  }
  if (last < rest.length) out.push(rest.slice(last));
  return out;
}

// MrkdwnSource shows a mrkdwn message exactly as it is sent, unrendered, with
// syntax colouring: markers, link targets and labels are told apart, so the
// source reads as easily as the rendered preview.
export function MrkdwnSource({ text, className }: { text: string; className?: string }) {
  return (
    <pre className={cn("m-0 overflow-auto rounded-md border border-border bg-[var(--fx-control)] p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap break-words text-muted-foreground", className)}>
      {text.split("\n").map((line, i) => (
        <span key={i}>
          {highlightLine(line, String(i))}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}
