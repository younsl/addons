import type { ComponentProps } from "react";
import ReactMarkdown from "react-markdown";
import remarkGemoji from "remark-gemoji";
import remarkGfm from "remark-gfm";
import { CodeView } from "@/components/app-ui/code-view";

// DocLink opens document links in a new tab so a README never navigates the
// console away.
function DocLink(props: ComponentProps<"a">) {
  const { children, ...rest } = props;
  return (
    <a {...rest} target="_blank" rel="noreferrer">
      {children}
    </a>
  );
}

// DocCode highlights fenced yaml/json blocks with the shared CodeView; other
// languages render as plain preformatted blocks.
function DocPre(props: ComponentProps<"pre">) {
  const child = props.children as { props?: { className?: string; children?: string } } | undefined;
  const cls = child?.props?.className ?? "";
  const code = typeof child?.props?.children === "string" ? child.props.children.replace(/\n$/, "") : "";
  if (code && /language-(yaml|yml)/.test(cls)) return <CodeView code={code} language="yaml" />;
  if (code && /language-json/.test(cls)) return <CodeView code={code} language="json" />;
  return <pre {...props} />;
}

// MarkdownDoc renders a full document (a chart README) with GitHub-style
// typography: the spacing, heading rules, table borders and code styling of
// github-markdown-css, expressed as scoped Tailwind arbitrary variants so no
// global stylesheet is imported. react-markdown never emits raw HTML from the
// source, so a README cannot inject markup.
export function MarkdownDoc({ source }: { source: string }) {
  return (
    <div
      className={[
        "min-w-0 break-words text-sm leading-relaxed text-foreground",
        // headings: GitHub scale, h1/h2 underlined
        "[&_h1]:mb-4 [&_h1]:mt-6 [&_h1]:border-b [&_h1]:border-border [&_h1]:pb-2 [&_h1]:text-2xl [&_h1]:font-semibold [&_h1]:leading-tight [&_h1:first-child]:mt-0",
        "[&_h2]:mb-4 [&_h2]:mt-6 [&_h2]:border-b [&_h2]:border-border [&_h2]:pb-2 [&_h2]:text-xl [&_h2]:font-semibold [&_h2]:leading-tight [&_h2:first-child]:mt-0",
        "[&_h3]:mb-3 [&_h3]:mt-6 [&_h3]:text-lg [&_h3]:font-semibold",
        "[&_h4]:mb-3 [&_h4]:mt-6 [&_h4]:text-base [&_h4]:font-semibold",
        "[&_h5]:mb-2 [&_h5]:mt-5 [&_h5]:text-sm [&_h5]:font-semibold",
        "[&_h6]:mb-2 [&_h6]:mt-5 [&_h6]:text-sm [&_h6]:font-semibold [&_h6]:text-muted-foreground",
        // block spacing
        "[&_p]:mb-4 [&_p]:mt-0",
        "[&_ul]:mb-4 [&_ul]:mt-0 [&_ul]:list-disc [&_ul]:pl-8",
        "[&_ol]:mb-4 [&_ol]:mt-0 [&_ol]:list-decimal [&_ol]:pl-8",
        "[&_li]:mt-1 [&_li>p]:mb-2",
        "[&_ul_ul]:mb-0 [&_ol_ol]:mb-0 [&_ul_ol]:mb-0 [&_ol_ul]:mb-0",
        "[&_hr]:my-6 [&_hr]:border-t [&_hr]:border-border",
        // blockquote
        "[&_blockquote]:mb-4 [&_blockquote]:border-l-4 [&_blockquote]:border-border [&_blockquote]:pl-4 [&_blockquote]:text-muted-foreground",
        // inline + block code
        "[&_code]:rounded-md [&_code]:bg-muted [&_code]:px-1.5 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-[85%]",
        "[&_pre]:mb-4 [&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:bg-muted [&_pre]:p-4 [&_pre]:leading-normal",
        "[&_pre_code]:block [&_pre_code]:bg-transparent [&_pre_code]:p-0 [&_pre_code]:text-xs",
        // tables: full borders + zebra rows, wrapped for overflow
        "[&_table]:mb-4 [&_table]:block [&_table]:max-w-full [&_table]:overflow-x-auto [&_table]:border-collapse",
        "[&_th]:border [&_th]:border-border [&_th]:px-3 [&_th]:py-1.5 [&_th]:text-left [&_th]:font-semibold",
        "[&_td]:border [&_td]:border-border [&_td]:px-3 [&_td]:py-1.5",
        "[&_tbody_tr:nth-child(2n)]:bg-muted/40",
        // media + links
        "[&_img]:max-w-full [&_img]:rounded-md",
        "[&_a]:font-medium [&_a]:text-accent-ink [&_a]:no-underline hover:[&_a]:underline",
      ].join(" ")}
    >
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkGemoji]}
        components={{ a: DocLink, pre: DocPre }}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}
