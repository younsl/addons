import { render } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { renderMrkdwn } from "@/routes/workspace/repositories/-components/detail/policy-flow";

describe("renderMrkdwn", () => {
  test("renders a link inside bold, as alarm titles use", () => {
    const { container } = render(<>{renderMrkdwn("*<https://forklift.example.com/workspace/insights|Forklift security insights>*")}</>);
    const link = container.querySelector("strong a");
    expect(link?.getAttribute("href")).toBe("https://forklift.example.com/workspace/insights");
    expect(link?.textContent).toBe("Forklift security insights");
  });

  test("keeps plain bold, plain links and unsafe schemes apart", () => {
    const { container } = render(<>{renderMrkdwn("*Findings*: 1. <https://a.example.com/x|One> and <javascript:alert(1)|Bad>")}</>);
    expect(container.querySelector("strong")?.textContent).toBe("Findings");
    const links = container.querySelectorAll("a");
    expect(links).toHaveLength(1);
    expect(links[0].textContent).toBe("One");
    expect(container.textContent).toContain("Bad");
  });
});

describe("renderMrkdwn lists", () => {
  test("renders a run of numbered lines as one ordered list", () => {
    const { container } = render(<>{renderMrkdwn("*Findings*\n1. <https://a.example.com/x|One> (high)\n2. Two\n_1 more_")}</>);
    const items = container.querySelectorAll("ol > li");
    expect(items).toHaveLength(2);
    expect(items[0].querySelector("a")?.textContent).toBe("One");
    expect(items[1].textContent).toBe("Two");
    expect(container.querySelectorAll("ol")).toHaveLength(1);
  });
});
