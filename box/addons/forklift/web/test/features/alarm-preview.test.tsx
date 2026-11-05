import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, test, vi } from "vitest";

import { AlarmPreview } from "@/components/app-ui/alarm-preview";
import { useUserPreferences } from "@/stores/user-preferences";

const TEXT = "*Forklift coverage report*\n1. payments/checkout-api";

describe("AlarmPreview", () => {
  beforeEach(() => {
    useUserPreferences.setState({ language: "en" });
  });

  test("copies the raw text from the Raw tab only", async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    render(<AlarmPreview text={TEXT} />);

    expect(screen.queryByRole("button", { name: "Copy" })).not.toBeInTheDocument();
    expect(screen.getByRole("listitem")).toHaveTextContent("payments/checkout-api");

    await user.click(screen.getByRole("tab", { name: "Raw" }));
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith(TEXT);
    expect(screen.getByRole("button", { name: "Copied" })).toBeVisible();
  });
});
