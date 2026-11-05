import { Fragment, type ReactNode } from "react";
import { cn } from "@/lib/utils";

// Line-based GitLab CI viewer, ported from the Backstage plugin.
//
// A real YAML parse is not worth it here, and a prebuilt highlighter is not
// usable: the point of this view is to mark the lines that reference forklift,
// which needs one row per line so a row can carry its own background. A
// per-line tokenizer keeps that structure intact.

// Keywords that only ever appear at the top level of a pipeline.
const GLOBAL_KEYWORDS = ["default", "include", "stages", "variables", "workflow"];

// Keywords GitLab reserves inside a job, plus the shared global ones.
const CI_KEYWORDS = new Set([
  ...GLOBAL_KEYWORDS,
  "after_script", "allow_failure", "artifacts", "before_script", "cache",
  "coverage", "dast_configuration", "dependencies", "environment", "except",
  "extends", "hooks", "id_tokens", "image", "inherit", "interruptible", "needs",
  "only", "pages", "parallel", "publish", "release", "resource_group", "retry",
  "rules", "script", "secrets", "services", "stage", "tags", "timeout",
  "trigger", "when",
]);

// Splits $VAR, ${VAR} and $CI_... out of a value.
const VARIABLE_RE = /(\$\{[A-Za-z_][A-Za-z0-9_]*\}|\$[A-Za-z_][A-Za-z0-9_]*)/g;

// The forklift credential a job authenticates with, which counts as a reference
// even when the host is never named. Kept in step with tokenRe in
// src/coverage/scan.rs, which is what decides the verdict.
const TOKEN_RE = /FORKLIFT_[A-Z0-9_]*TOKEN/;

const TOKEN_CLASS = {
  job: "text-[var(--fx-code-job)] font-semibold",
  keyword: "text-[var(--fx-code-keyword)]",
  key: "text-[var(--fx-code-key)]",
  string: "text-[var(--fx-code-string)]",
  variable: "text-[var(--fx-code-variable)]",
  literal: "text-[var(--fx-code-literal)]",
  number: "text-[var(--fx-code-number)]",
  comment: "text-[var(--fx-code-comment)]",
  punct: "text-[var(--fx-code-punct)]",
} as const;

function withVariables(text: string, keyPrefix: string): ReactNode[] {
  // split() on a regex with one capture group yields text, capture, text, ...
  // so odd indices are the variables. Testing the global regex per part would be
  // wrong, since its lastIndex carries between calls.
  return text.split(VARIABLE_RE).map((part, index) =>
    index % 2 === 1 ? (
      <span key={`${keyPrefix}-v${index}`} className={TOKEN_CLASS.variable}>
        {part}
      </span>
    ) : (
      <Fragment key={`${keyPrefix}-t${index}`}>{part}</Fragment>
    )
  );
}

function highlightValue(raw: string, keyPrefix: string): ReactNode {
  if (raw.trim() === "") return raw;

  // Trailing comments are common on script lines, so split them off first.
  const commentAt = raw.search(/(^|\s)#/);
  if (commentAt >= 0) {
    return (
      <>
        {highlightValue(raw.slice(0, commentAt), `${keyPrefix}-b`)}
        <span className={TOKEN_CLASS.comment}>{raw.slice(commentAt)}</span>
      </>
    );
  }

  const leading = raw.slice(0, raw.length - raw.trimStart().length);
  const value = raw.trim();

  let cls: string = TOKEN_CLASS.string;
  if (/^(true|false|null|~)$/i.test(value)) cls = TOKEN_CLASS.literal;
  else if (/^-?\d+(\.\d+)?$/.test(value)) cls = TOKEN_CLASS.number;
  else if (/^[&*][A-Za-z0-9_-]+$/.test(value)) cls = cn(TOKEN_CLASS.literal, "italic");

  return (
    <>
      {leading}
      <span className={cls}>{withVariables(value, keyPrefix)}</span>
    </>
  );
}

export function highlightGitlabCiLine(line: string, keyPrefix: string): ReactNode {
  if (line.trim() === "") return line;
  if (/^\s*#/.test(line)) return <span className={TOKEN_CLASS.comment}>{line}</span>;

  const listMatch = line.match(/^(\s*)(-\s+)(.*)$/);
  if (listMatch) {
    const [, indent, dash, rest] = listMatch;
    // A list entry can itself be a mapping, e.g. `- name: build`.
    const inner = rest.match(/^([\w.$/-]+)(:)(\s*)(.*)$/);
    return (
      <>
        {indent}
        <span className={TOKEN_CLASS.punct}>{dash}</span>
        {inner ? (
          <>
            <span className={TOKEN_CLASS.key}>{inner[1]}</span>
            <span className={TOKEN_CLASS.punct}>{inner[2]}</span>
            {inner[3]}
            {highlightValue(inner[4], `${keyPrefix}-li`)}
          </>
        ) : (
          highlightValue(rest, `${keyPrefix}-l`)
        )}
      </>
    );
  }

  const kvMatch = line.match(/^(\s*)([\w.$/-]+)(:)(.*)$/);
  if (kvMatch) {
    const [, indent, key, colon, rest] = kvMatch;
    let keyClass: string = TOKEN_CLASS.key;
    if (CI_KEYWORDS.has(key)) keyClass = TOKEN_CLASS.keyword;
    // A top-level key that is not a reserved word is a job name, the anchor most
    // people scan the file for.
    else if (indent.length === 0) keyClass = TOKEN_CLASS.job;

    return (
      <>
        {indent}
        <span className={keyClass}>{key}</span>
        <span className={TOKEN_CLASS.punct}>{colon}</span>
        {highlightValue(rest, keyPrefix)}
      </>
    );
  }

  return highlightValue(line, keyPrefix);
}

// lineReferencesForklift is why a line is marked: it names the forklift host, or
// it uses the forklift credential. Same rule the scanner applied to the file.
export function lineReferencesForklift(line: string, forkliftHost: string): boolean {
  if (forkliftHost !== "" && line.includes(forkliftHost)) return true;
  return TOKEN_RE.test(line);
}

// GitlabCiView renders one file, one row per line, with the rows that reference
// forklift given their own background. That marking is the whole reason this
// view exists: it shows which lines the verdict was actually based on.
export function GitlabCiView({
  path,
  content,
  forkliftHost,
}: {
  path: string;
  content: string;
  forkliftHost: string;
}) {
  const lines = content.replace(/\n$/, "").split("\n");
  return (
    <pre className="m-0 max-h-[30rem] overflow-auto rounded-md border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel-raised)] py-2 font-mono text-[12px] leading-6">
      {lines.map((line, index) => {
        const hit = lineReferencesForklift(line, forkliftHost);
        return (
          <div
            key={`${path}-${index}`}
            className={cn("flex gap-3 whitespace-pre px-3", hit && "bg-[var(--fx-code-hit)]")}
          >
            <span className="min-w-8 shrink-0 select-none text-right text-[var(--fx-text-subtle)]">
              {index + 1}
            </span>
            <span className="flex-1">
              {line ? highlightGitlabCiLine(line, `${path}-${index}`) : " "}
            </span>
          </div>
        );
      })}
    </pre>
  );
}
