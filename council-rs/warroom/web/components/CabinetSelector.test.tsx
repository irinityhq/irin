import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { Cabinet, HealthResponse } from "@/lib/types";
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

const health: HealthResponse = {
  council_version: "test",
  stream_version: "1",
  providers_available: ["nvidia"],
  providers_missing: ["grok_hermes", "gemini_agy", "claude_code", "nous"],
  sessions_dir: "/tmp",
  index_path: "/tmp/index",
  index_exists: false,
};

describe("CabinetSelector unavailable treatment", () => {
  it("mutes unavailable cards without danger-red requirement prose", () => {
    const html = renderToStaticMarkup(
      <CabinetSelector
        cabinets={cabinets}
        selected="standard"
        onSelect={() => {}}
        health={health}
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
        health={health}
      />,
    );

    const needBlocks = html.match(/data-testid="cabinet-need"/g) ?? [];
    expect(needBlocks.length).toBeGreaterThanOrEqual(2);
    // No text-danger adjacent to any need span.
    expect(html).not.toContain('text-danger ml-1');
    expect(html).not.toContain('class="text-danger');
  });
});
