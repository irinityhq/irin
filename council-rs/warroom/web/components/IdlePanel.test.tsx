import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import type { Cabinet, DiscoverProvider, DiscoverResponse } from "@/lib/types";
import {
  __loadDiscoverForTests,
  __resetDiscoverForTests,
} from "@/lib/use-discover";
import IdlePanel from "./IdlePanel";
import { ToastProvider } from "./Toast";

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

function provider(name: string, available: boolean): DiscoverProvider {
  return {
    name,
    label: name,
    family: "test",
    transport: name,
    available,
    gateway_supported: true,
    source: "test",
    env_hint: null,
    models: [],
  };
}

async function preloadDiscover(payload: DiscoverResponse): Promise<void> {
  __resetDiscoverForTests({ fetchOnce: async () => payload });
  await __loadDiscoverForTests();
}

function renderIdle(cabinets: Cabinet[], initialCabinet?: string): string {
  return renderToStaticMarkup(
    <ToastProvider>
      <IdlePanel
        variant="shell"
        cabinets={cabinets}
        onStart={() => {}}
        initialCabinet={initialCabinet ?? null}
      />
    </ToastProvider>,
  );
}

afterEach(() => {
  __resetDiscoverForTests(null);
});

describe("IdlePanel convene gating follows the Discover inventory", () => {
  it("CLI-only host: convene is unblocked with a runnable CLI cabinet selected", async () => {
    // The review finding: only claude/codex CLIs are installed. /api/health
    // (a no-CLI liveness probe) reports those transports missing, but
    // /api/discover proves them available. Gating must follow Discover, so
    // no provider blocker may render. (The topic-length half of canStart is
    // unchanged and covered by e2e; static markup always renders topic="".)
    await preloadDiscover({
      providers: [
        provider("claude_code", true),
        provider("codex_cli", true),
        provider("grok_hermes", false),
        provider("gemini_agy", false),
      ],
      log: [],
    });
    const cabinets = [
      cab("standard", ["grok_hermes", "gemini_agy", "claude_code"]),
      cab("cli-review", ["claude_code", "codex_cli"]),
    ];

    // initialCabinet stands in for the post-auto-select steady state (the
    // effect itself is pinned in lib/cabinet-selection.test.ts).
    const html = renderIdle(cabinets, "cli-review");

    expect(html).not.toContain("No cabinet has all required providers");
    expect(html).not.toContain('data-testid="provider-selection-warning"');
    expect(html).toContain("Convene the Council");

    // Cabinet chips read the same Discover inventory: CLI cabinet runnable,
    // API-only standard muted with its requirement list.
    expect(html).toContain('data-cabinet-name="cli-review" data-cabinet-available="true"');
    expect(html).toContain('data-cabinet-name="standard" data-cabinet-available="false"');
  });

  it("explains when no cabinet is runnable under the Discover inventory", async () => {
    await preloadDiscover({
      providers: [
        provider("claude_code", false),
        provider("grok_hermes", false),
        provider("gemini_agy", false),
      ],
      log: [],
    });
    const cabinets = [cab("standard", ["grok_hermes", "gemini_agy", "claude_code"])];

    const html = renderIdle(cabinets, "standard");

    expect(html).toContain('data-testid="provider-selection-warning"');
    expect(html).toContain("No cabinet has all required providers available");
  });

  it("waits for the Discover inventory instead of gating on health", () => {
    // Fresh reset: cache empty, no in-flight — the hook reports loading.
    __resetDiscoverForTests(null);
    const cabinets = [cab("standard", ["grok_hermes", "gemini_agy", "claude_code"])];

    const html = renderIdle(cabinets, "standard");

    expect(html).toContain("Provider availability is still being checked.");
  });
});
