import { describe, expect, it } from "vitest";
import type { Cabinet, DiscoverProvider, DiscoverResponse } from "./types";
import {
  DEFAULT_CABINET_NAME,
  isCabinetRunnable,
  noRunnableCabinetExplanation,
  pickRunnableCabinetName,
  resolveUntouchedCabinetSelection,
} from "./cabinet-selection";
import { availableProviderIds } from "./use-discover";

function cab(
  name: string,
  providers: string[],
  opts: { triad?: boolean; label?: string } = {},
): Cabinet {
  const [chair, ...seats] = providers.length > 0 ? providers : ["missing"];
  return {
    name,
    label: opts.label ?? name,
    description: "",
    seats: seats.map((provider, i) => ({
      name: `seat-${i}`,
      provider,
      model: "m",
      system: "",
    })),
    chair: { provider: chair, model: "m" },
    rounds: 1,
    is_triad: opts.triad ?? false,
  };
}

const standard = cab("standard", ["grok_hermes", "gemini_agy", "claude_code"], {
  label: "Standard Council",
});
const starter = cab("starter-nvidia", ["nvidia"], { label: "starter-nvidia" });
const freeride = cab("freeride", ["nous"], { label: "freeride" });
const triad = cab("triad-risk", ["grok_hermes", "claude_code"], { triad: true });

describe("isCabinetRunnable", () => {
  it("requires every seat and chair transport", () => {
    expect(isCabinetRunnable(standard, ["grok_hermes", "gemini_agy", "claude_code"])).toBe(
      true,
    );
    expect(isCabinetRunnable(standard, ["grok_hermes", "claude_code"])).toBe(false);
    expect(isCabinetRunnable(standard, null)).toBe(false);
  });
});

describe("pickRunnableCabinetName", () => {
  it("keeps the preferred default when it is runnable", () => {
    expect(
      pickRunnableCabinetName(
        [standard, starter],
        ["grok_hermes", "gemini_agy", "claude_code", "nvidia"],
      ),
    ).toBe("standard");
  });

  it("picks the first runnable non-triad cabinet in list order when default is blocked", () => {
    // API order: standard (blocked), freeride (blocked), starter (ok), triad (ok)
    expect(
      pickRunnableCabinetName(
        [standard, freeride, starter, triad],
        ["nvidia", "grok_hermes", "claude_code"],
      ),
    ).toBe("starter-nvidia");
  });

  it("falls through to a triad only when no embedded cabinet is runnable", () => {
    expect(
      pickRunnableCabinetName([standard, triad], ["grok_hermes", "claude_code"]),
    ).toBe("triad-risk");
  });

  it("returns null when nothing is runnable", () => {
    expect(pickRunnableCabinetName([standard, starter], ["openai_api"])).toBeNull();
  });
});

describe("resolveUntouchedCabinetSelection", () => {
  it("auto-selects a stable runnable cabinet on untouched first load", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [standard, starter, freeride],
        providersAvailable: ["nvidia"],
        currentName: DEFAULT_CABINET_NAME,
        selectionLocked: false,
      }),
    ).toBe("starter-nvidia");
  });

  it("preserves an explicit / locked selection even when unavailable", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [standard, starter],
        providersAvailable: ["nvidia"],
        currentName: "standard",
        selectionLocked: true,
      }),
    ).toBeNull();
  });

  it("does not change selection when the current cabinet is already runnable", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [standard, starter],
        providersAvailable: ["grok_hermes", "gemini_agy", "claude_code", "nvidia"],
        currentName: "standard",
        selectionLocked: false,
      }),
    ).toBeNull();
  });

  it("waits until health inventory is known", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [standard, starter],
        providersAvailable: null,
        currentName: "standard",
        selectionLocked: false,
      }),
    ).toBeNull();
  });

  it("waits until the cabinet list is non-empty", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [],
        providersAvailable: ["nvidia"],
        currentName: "standard",
        selectionLocked: false,
      }),
    ).toBeNull();
  });

  it("keeps selection when no cabinet is runnable (caller explains once)", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets: [standard, starter],
        providersAvailable: [],
        currentName: "standard",
        selectionLocked: false,
      }),
    ).toBeNull();
  });
});

describe("noRunnableCabinetExplanation", () => {
  it("returns one actionable line when every cabinet is blocked", () => {
    const msg = noRunnableCabinetExplanation([standard, starter], []);
    expect(msg).toMatch(/No cabinet has all required providers/i);
    expect(msg).toMatch(/Discover/i);
  });

  it("is silent when at least one cabinet is runnable", () => {
    expect(noRunnableCabinetExplanation([standard, starter], ["nvidia"])).toBeNull();
  });
});

function discoverPayload(providers: Array<[string, boolean]>): DiscoverResponse {
  return {
    providers: providers.map(
      ([name, available]): DiscoverProvider => ({
        name,
        label: name,
        family: "test",
        transport: name,
        available,
        gateway_supported: true,
        source: "test",
        env_hint: null,
        models: [],
      }),
    ),
    log: [],
  };
}

describe("availableProviderIds", () => {
  it("is null while the Discover inventory is unknown", () => {
    expect(availableProviderIds(null)).toBeNull();
  });

  it("lists only transports discovery proved available", () => {
    const ids = availableProviderIds(
      discoverPayload([
        ["claude_code", true],
        ["codex_cli", true],
        ["grok_hermes", false],
      ]),
    );
    expect(ids).toEqual(["claude_code", "codex_cli"]);
  });
});

describe("Discover-sourced runnability (CLI-only operator)", () => {
  // The finding: a host with only claude/codex CLIs installed. /api/health is
  // a no-CLI liveness probe and reports those transports missing; /api/discover
  // proves them available. Gating must follow Discover.
  const cliCabinet = cab("cli-review", ["claude_code", "codex_cli"], {
    label: "CLI Review",
  });
  // standard needs grok_hermes + gemini_agy (unavailable) → blocked.
  const cabinets = [standard, cliCabinet];
  const ids = availableProviderIds(
    discoverPayload([
      ["claude_code", true],
      ["codex_cli", true],
      ["grok_hermes", false],
      ["gemini_agy", false],
    ]),
  );

  it("treats the CLI cabinet as runnable and the API cabinet as blocked", () => {
    expect(isCabinetRunnable(cliCabinet, ids)).toBe(true);
    expect(isCabinetRunnable(standard, ids)).toBe(false);
  });

  it("auto-selects the runnable CLI cabinet off the blocked default", () => {
    expect(
      resolveUntouchedCabinetSelection({
        cabinets,
        providersAvailable: ids,
        currentName: DEFAULT_CABINET_NAME,
        selectionLocked: false,
      }),
    ).toBe("cli-review");
  });

  it("never raises the no-runnable-cabinet explanation", () => {
    expect(noRunnableCabinetExplanation(cabinets, ids)).toBeNull();
  });
});
