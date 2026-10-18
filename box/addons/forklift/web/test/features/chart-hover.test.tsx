import { fireEvent, render } from "@testing-library/react";
import { describe, expect, test, vi } from "vitest";

import { ChartHover } from "@/routes/workspace/insights/-components/chart-hover";

describe("ChartHover drag to zoom", () => {
  const setup = () => {
    const onSelect = vi.fn();
    const { container } = render(
      <div style={{ position: "relative", width: 100, height: 50 }}>
        <ChartHover n={10} layout="slot" index={null} onIndex={() => {}} onSelect={onSelect} />
      </div>,
    );
    const overlay = container.querySelector(".cursor-crosshair") as HTMLDivElement;
    overlay.getBoundingClientRect = () => ({ left: 0, top: 0, width: 100, height: 50, right: 100, bottom: 50, x: 0, y: 0, toJSON: () => ({}) });
    return { overlay, onSelect };
  };

  test("hands back the first and last run a drag spans", () => {
    const { overlay, onSelect } = setup();
    fireEvent.pointerDown(overlay, { clientX: 75, button: 0, pointerId: 1 });
    fireEvent.pointerMove(overlay, { clientX: 25, pointerId: 1 });
    fireEvent.pointerUp(overlay, { clientX: 25, pointerId: 1 });
    expect(onSelect).toHaveBeenCalledWith(2, 7);
  });

  test("a click without a drag does not zoom", () => {
    const { overlay, onSelect } = setup();
    fireEvent.pointerDown(overlay, { clientX: 40, button: 0, pointerId: 1 });
    fireEvent.pointerUp(overlay, { clientX: 40, pointerId: 1 });
    expect(onSelect).not.toHaveBeenCalled();
  });
});
