import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ExecutionRouteChip } from "./ModeChips";

describe("ExecutionRouteChip", () => {
  it("shows the governed route and sensitivity", () => {
    const html = renderToStaticMarkup(
      <ExecutionRouteChip route="governed" sensitivity="yellow" />,
    );
    expect(html).toContain("GOVERNED · YELLOW");
    expect(html).toContain("execution-route-chip");
  });

  it("labels direct proceedings without implying governance", () => {
    const html = renderToStaticMarkup(<ExecutionRouteChip route="direct" />);
    expect(html).toContain("DIRECT");
    expect(html).not.toContain("GOVERNED");
  });

  it("hides the route for legacy sessions", () => {
    expect(renderToStaticMarkup(<ExecutionRouteChip route="unknown" />)).toBe("");
  });
});
