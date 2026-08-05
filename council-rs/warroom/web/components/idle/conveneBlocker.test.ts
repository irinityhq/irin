import { describe, expect, it } from "vitest";
import type { Cabinet, DiscoverProvider, DiscoverResponse } from "@/lib/types";
import { conveneBlocker } from "./conveneBlocker";

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

function provider(
  name: string,
  available: boolean,
  gateway_supported = true,
): DiscoverProvider {
  return {
    name,
    label: name,
    family: "test",
    transport: name,
    available,
    gateway_supported,
    source: "test",
    env_hint: null,
    models: [],
  };
}

function baseArgs(overrides: Partial<Parameters<typeof conveneBlocker>[0]> = {}) {
  const cabinets = [cab("standard", ["grok_hermes", "claude_code"])];
  const providerOptions = [
    provider("grok_hermes", true),
    provider("claude_code", true),
  ];
  const discoverData: DiscoverResponse = { providers: providerOptions, log: [] };
  return {
    discoverData,
    discoverLoading: false,
    discoverError: null as string | null,
    cabinets,
    availableIds: ["grok_hermes", "claude_code"] as string[] | null,
    cabinet: cabinets[0] as Cabinet | undefined,
    providerOptions,
    validate: false,
    validateProvider: "grok_build",
    viaGateway: false,
    ...overrides,
  };
}

describe("conveneBlocker", () => {
  it("returns null when discovery is ready and the cabinet is runnable", () => {
    expect(conveneBlocker(baseArgs())).toBeNull();
  });

  it("prefers discovery failure over loading or inventory gaps", () => {
    expect(
      conveneBlocker(
        baseArgs({
          discoverError: "timeout",
          discoverLoading: true,
          discoverData: null,
          availableIds: null,
        }),
      ),
    ).toBe("Provider discovery failed: timeout");
  });

  it("blocks while discovery is still loading or empty", () => {
    expect(
      conveneBlocker(
        baseArgs({
          discoverData: null,
          discoverLoading: true,
        }),
      ),
    ).toBe("Provider availability is still being checked.");

    expect(
      conveneBlocker(
        baseArgs({
          discoverData: null,
          discoverLoading: false,
        }),
      ),
    ).toBe("Provider availability is still being checked.");
  });

  it("explains when no cabinet is runnable under the Discover inventory", () => {
    const cabinets = [cab("standard", ["grok_hermes", "claude_code"])];
    const providerOptions = [
      provider("grok_hermes", false),
      provider("claude_code", false),
    ];
    expect(
      conveneBlocker(
        baseArgs({
          cabinets,
          cabinet: cabinets[0],
          providerOptions,
          availableIds: [],
          discoverData: { providers: providerOptions, log: [] },
        }),
      ),
    ).toMatch(/^No cabinet has all required providers available/);
  });

  it("blocks on selected-cabinet provider gaps when some other cabinet is runnable", () => {
    const runnable = cab("cli-review", ["claude_code", "codex_cli"]);
    const blocked = cab("standard", ["grok_hermes", "claude_code"]);
    const providerOptions = [
      provider("claude_code", true),
      provider("codex_cli", true),
      provider("grok_hermes", false),
    ];
    expect(
      conveneBlocker(
        baseArgs({
          cabinets: [blocked, runnable],
          cabinet: blocked,
          providerOptions,
          availableIds: ["claude_code", "codex_cli"],
          discoverData: { providers: providerOptions, log: [] },
        }),
      ),
    ).toMatch(/Unavailable or legacy provider transport/);
  });

  it("blocks on validator provider when validation is enabled", () => {
    const providerOptions = [
      provider("grok_hermes", true),
      provider("claude_code", true),
      provider("grok_build", false),
    ];
    expect(
      conveneBlocker(
        baseArgs({
          validate: true,
          validateProvider: "grok_build",
          providerOptions,
          discoverData: { providers: providerOptions, log: [] },
        }),
      ),
    ).toMatch(/Unavailable or legacy provider transport.*grok_build/);
  });

  it("blocks on gateway-unsupported transports only when viaGateway is on", () => {
    const providerOptions = [
      provider("grok_hermes", true, false),
      provider("claude_code", true, true),
    ];
    const args = baseArgs({
      providerOptions,
      discoverData: { providers: providerOptions, log: [] },
      viaGateway: false,
    });
    expect(conveneBlocker(args)).toBeNull();
    expect(conveneBlocker({ ...args, viaGateway: true })).toMatch(
      /Gateway has no adapter for transport/,
    );
  });

  it("reports missing selected cabinet after discovery succeeds", () => {
    expect(
      conveneBlocker(
        baseArgs({
          cabinet: undefined,
        }),
      ),
    ).toBe("Selected cabinet was not found.");
  });
});
