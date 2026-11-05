import { render } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { highlightMarkdown } from "@/components/app-ui/markdown-editor";

const SOURCE = [
  "# Maintenance :warning:",
  "> **tonight** 22:00, see [status](https://status.example.com) or https://example.com",
  "- `npm` and _pip_ pulls may be slow",
  "1. retry later",
  "```",
  "pip install typing_extensions",
  "```",
].join("\n");

describe("highlightMarkdown", () => {
  test("keeps every character of the source", () => {
    const { container } = render(<pre>{highlightMarkdown(SOURCE)}</pre>);
    expect(container.textContent).toBe(`${SOURCE}\n`);
  });

  test("colours the Markdown syntax", () => {
    const { container } = render(<pre>{highlightMarkdown(SOURCE)}</pre>);
    const coloured = (text: string) => [...container.querySelectorAll("span[class]")].some((s) => s.textContent === text);
    for (const token of ["# ", ":warning:", "**", "`npm`", "https://status.example.com", "-", "1.", "pip install typing_extensions"]) {
      expect(coloured(token), token).toBe(true);
    }
  });
});
