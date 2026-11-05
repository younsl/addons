import { render } from "@testing-library/react";
import ReactMarkdown from "react-markdown";
import { describe, expect, test } from "vitest";

import { remarkLineBreaks } from "@/lib/remark-line-breaks";

const md = (source: string) => render(<ReactMarkdown remarkPlugins={[remarkLineBreaks]}>{source}</ReactMarkdown>).container;

describe("remarkLineBreaks", () => {
  test("turns a single newline into a line break", () => {
    const html = md("**Maintenance** tonight\nPulls may be slow").innerHTML;
    expect(html).toContain("<br>");
    expect(html).toContain("Pulls may be slow");
  });

  test("keeps newlines inside code blocks", () => {
    const container = md("```\nline one\nline two\n```");
    expect(container.querySelector("br")).toBeNull();
    expect(container.querySelector("code")?.textContent).toBe("line one\nline two\n");
  });

  test("still separates paragraphs on a blank line", () => {
    expect(md("one\n\ntwo").querySelectorAll("p")).toHaveLength(2);
  });
});
