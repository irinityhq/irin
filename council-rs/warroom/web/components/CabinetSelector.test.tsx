import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { Cabinet } from "@/lib/types";
import CabinetSelector from "./CabinetSelector";

function cab(name: string, providers: string[]): Cabinet {
  const [chair, ...seats] = providers;
  return {
    name,
    label: name,
    description: "",
    seats: seats.map((provider, i) => ({
      name: `s${i}`,
      provider,
      model: "m",
      system: "",
    })),
    chair: { provider: chair, model: "m" },
    rounds: 2,
    is_triad: false,
  };
}

const cabinets: Cabinet[] = [
  // providers: [chair, ...seats]
  cab("standard", ["grok_hermes", "gemini_agy", "claude_code"]),
  cab("starter-nvidia", ["nvidia", "nvidia"]),
  cab("freeride", ["nous", "nous"]),
];

// Discover-inventory availability (the only runnability source): only the
// nvidia transport is available in this scenario.
const providersAvailable = ["nvidia"];

describe("CabinetSelector unavailable treatment", () => {
  it("mutes unavailable cards without danger-red requirement prose", () => {
    const html = renderToStaticMarkup(
      <CabinetSelector
        cabinets={cabinets}
        selected="standard"
        onSelect={() => {}}
        providersAvailable={providersAvailable}
      />,
    );

    expect(html).toContain('data-cabinet-name="standard"');
    expect(html).toContain('data-cabinet-available="false"');
    expect(html).toContain('data-cabinet-name="starter-nvidia"');
    expect(html).toContain('data-cabinet-available="true"');

    // Requirements remain visible for operator diagnosis (seat order then chair).
    expect(html).toContain("(need gemini_agy, claude_code, grok_hermes)");
    // …but not as danger-red grid chrome.
    expect(html).not.toMatch(/text-danger[^"]*"[^>]*>\(need /);
    expect(html).not.toMatch(/\(need [^)]*\)[^<]*text-danger/);
    // Need labels use muted class.
    expect(html).toMatch(/text-fg-muted[^"]*"[^>]*data-testid="cabinet-need"/);
  });

  it("does not paint a danger Christmas tree across the unavailable grid", () => {
    const html = renderToStaticMarkup(
      <CabinetSelector
        cabinets={cabinets}
        selected="starter-nvidia"
        onSelect={() => {}}
        providersAvailable={providersAvailable}
      />,
    );

    const needBlocks = html.match(/data-testid="cabinet-need"/g) ?? [];
    expect(needBlocks.length).toBeGreaterThanOrEqual(2);
    // No text-danger adjacent to any need span.
    expect(html).not.toContain('text-danger ml-1');
    expect(html).not.toContain('class="text-danger');
  });

  it("treats every cabinet as unknown while the Discover inventory is loading", () => {
    const html = renderToStaticMarkup(
      <CabinetSelector
        cabinets={cabinets}
        selected="standard"
        onSelect={() => {}}
        providersAvailable={null}
      />,
    );

    expect(html).not.toContain('data-cabinet-available="false"');
    expect(html).not.toContain("cabinet-need");
  });
});
