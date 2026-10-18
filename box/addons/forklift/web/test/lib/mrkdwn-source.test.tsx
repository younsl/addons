import { render } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { MrkdwnSource } from "@/components/app-ui/mrkdwn-source";

const TEXT = "*<https://forklift.example.com/workspace/insights|Forklift security insights>*\nScore 57 / 100 (D)\n\n1. <https://forklift.example.com/workspace/insights/x|[AC-05] Tokens> (high, 1 flagged)\n_note_";

describe("MrkdwnSource", () => {
  test("keeps the source text exactly as sent", () => {
    const { container } = render(<MrkdwnSource text={TEXT} />);
    expect(container.querySelector("pre")?.textContent).toBe(`${TEXT}\n`);
  });

  test("colours link targets, markers and list numbers", () => {
    const { container } = render(<MrkdwnSource text={TEXT} />);
    const urls = [...container.querySelectorAll("span.text-\\[var\\(--fx-info\\)\\]")].map((s) => s.textContent);
    expect(urls).toEqual([
      "https://forklift.example.com/workspace/insights",
      "https://forklift.example.com/workspace/insights/x",
    ]);
    const markers = [...container.querySelectorAll("span.text-\\[var\\(--fx-accent-ink\\)\\]")].map((s) => s.textContent);
    expect(markers).toEqual(["*", "*", "_", "_"]);
    expect(container.querySelector("span.text-\\[var\\(--fx-warning\\)\\]")?.textContent).toBe("1.");
  });
});
