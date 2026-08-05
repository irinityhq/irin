import { describe, expect, it, vi } from "vitest";
import type { Cabinet, MapmakerResult } from "@/lib/types";
import type { StartPayload } from "@/lib/ws";
import type { ConveneMatter } from "./useConveneMatter";
import type { ConveneOptions } from "./useConveneOptions";
// Pure payload factory despite the use* name — not a React hook.
import { useConveneSubmit as buildConveneSubmit } from "./useConveneSubmit";

function cabinet(overrides: Partial<Cabinet> = {}): Cabinet {
  return {
    name: "standard",
    label: "Standard",
    description: "",
    seats: [
      { name: "s0", provider: "claude_code", model: "m", system: "" },
    ],
    chair: { provider: "grok_hermes", model: "m" },
    rounds: 2,
    is_triad: false,
    ...overrides,
  };
}

function matter(overrides: Partial<ConveneMatter> = {}): ConveneMatter {
  return {
    topic: "Ship the fix",
    setTopic: () => {},
    context: "extra context",
    setContext: () => {},
    mapDir: "/tmp/map",
    setMapDir: () => {},
    mapBrief: null,
    setMapBrief: () => {},
    submitting: false,
    setSubmitting: () => {},
    ...overrides,
  };
}

function options(overrides: Partial<ConveneOptions> = {}): ConveneOptions {
  return {
    blind: false,
    setBlind: () => {},
    pause: true,
    setPause: () => {},
    maxRounds: "",
    setMaxRounds: () => {},
    mode: "teardown",
    setMode: () => {},
    validate: false,
    setValidate: () => {},
    validateProvider: "grok_build",
    setValidateProvider: () => {},
    validateGate: false,
    setValidateGate: () => {},
    frameCheck: true,
    setFrameCheck: () => {},
    scopeAuditor: false,
    setScopeAuditor: () => {},
    budgetUsd: "",
    setBudgetUsd: () => {},
    tier: "best",
    setTier: () => {},
    thenTearDown: false,
    setThenTearDown: () => {},
    specopsThreshold: 0.8,
    setSpecopsThreshold: () => {},
    workerProvJson: "",
    setWorkerProvJson: () => {},
    showAdvanced: false,
    setShowAdvanced: () => {},
    ...overrides,
  };
}

function submit(args: {
  canStart?: boolean;
  matter?: ConveneMatter;
  options?: ConveneOptions;
  cabinetName?: string;
  cabinet?: Cabinet;
  viaGateway?: boolean;
  sensitivity?: "green" | "yellow" | "red";
} = {}): {
  onStart: ReturnType<typeof vi.fn>;
  toast: ReturnType<typeof vi.fn>;
  setSubmitting: ReturnType<typeof vi.fn>;
  run: () => void;
} {
  const onStart = vi.fn();
  const toast = vi.fn();
  const setSubmitting = vi.fn();
  // Always capture setSubmitting so assertions can see the submit path arm it.
  const m = matter({
    ...(args.matter ?? {}),
    setSubmitting,
  });
  const run = buildConveneSubmit({
    canStart: args.canStart ?? true,
    matter: m,
    options: args.options ?? options(),
    cabinetName: args.cabinetName ?? "standard",
    cabinet: args.cabinet ?? cabinet(),
    viaGateway: args.viaGateway ?? false,
    sensitivity: args.sensitivity ?? "green",
    toast,
    onStart,
  });
  return { onStart, toast, setSubmitting, run };
}

function mapmakerResult(model: MapmakerResult["model"] = "grok"): MapmakerResult {
  return {
    model,
    model_id: "map-model",
    map: "",
    task: "",
    directory: "/tmp/map",
    file_count: 1,
    bundle_bytes: 0,
    tokens_in: 0,
    tokens_out: 0,
    cost_usd: 0,
    latency_ms: 0,
    brief_filename: null,
    brief_path: null,
  };
}

describe("useConveneSubmit", () => {
  it("no-ops when canStart is false or already submitting", () => {
    const blocked = submit({ canStart: false });
    blocked.run();
    expect(blocked.onStart).not.toHaveBeenCalled();

    const busy = submit({
      matter: matter({ submitting: true }),
    });
    busy.run();
    expect(busy.onStart).not.toHaveBeenCalled();
  });

  it("maps form state into a StartPayload with Direct routing by default", () => {
    const { onStart, setSubmitting, run } = submit({
      options: options({ maxRounds: 3, budgetUsd: 12.5, pause: true }),
    });
    run();

    expect(setSubmitting).toHaveBeenCalledWith(true);
    expect(onStart).toHaveBeenCalledTimes(1);
    const payload = onStart.mock.calls[0][0] as StartPayload;
    expect(payload).toMatchObject({
      topic: "Ship the fix",
      cabinet_name: "standard",
      context: "extra context",
      map_dir: "/tmp/map",
      blind: false,
      pause_after_each_round: true,
      max_rounds: 3,
      mode: "teardown",
      then_tear_down: false,
      budget_max_usd: 12.5,
      tier: "best",
      auto_specops_threshold: 0.8,
      validate: false,
      frame_check: true,
      via_gateway: false,
    });
    expect(payload.validate_provider).toBeUndefined();
    expect(payload.sensitivity).toBeUndefined();
  });

  it("injects Mapmaker brief as context and suppresses map_dir", () => {
    const { onStart, run } = submit({
      matter: matter({
        context: "operator note",
        mapDir: "/tmp/map",
        mapBrief: {
          text: "brief body",
          result: mapmakerResult("grok"),
        },
      }),
    });
    run();
    const payload = onStart.mock.calls[0][0] as StartPayload;
    expect(payload.map_dir).toBeUndefined();
    expect(payload.context).toContain("operator note");
    expect(payload.context).toContain("EXECUTION MAP (Mapmaker · grok)");
    expect(payload.context).toContain("brief body");
  });

  it("includes validate fields only when validation is enabled", () => {
    const off = submit({
      options: options({
        validate: false,
        validateProvider: "grok_build",
        validateGate: true,
      }),
    });
    off.run();
    expect((off.onStart.mock.calls[0][0] as StartPayload).validate_provider).toBeUndefined();
    expect((off.onStart.mock.calls[0][0] as StartPayload).validate_gate).toBeUndefined();

    const on = submit({
      options: options({
        validate: true,
        validateProvider: "claude_code",
        validateGate: true,
      }),
    });
    on.run();
    expect(on.onStart.mock.calls[0][0]).toMatchObject({
      validate: true,
      validate_provider: "claude_code",
      validate_gate: true,
    });
  });

  it("forces frame_check false for local_code_only cabinets", () => {
    const { onStart, run } = submit({
      cabinet: cabinet({ local_code_only: true }),
      options: options({ frameCheck: true }),
    });
    run();
    expect((onStart.mock.calls[0][0] as StartPayload).frame_check).toBe(false);
  });

  it("switches mode to pathfind when thenTearDown is set", () => {
    const { onStart, run } = submit({
      options: options({ mode: "harden", thenTearDown: true }),
    });
    run();
    expect(onStart.mock.calls[0][0]).toMatchObject({
      mode: "pathfind",
      then_tear_down: true,
    });
  });

  it("attaches Governed sensitivity when viaGateway is on", () => {
    const { onStart, run } = submit({
      viaGateway: true,
      sensitivity: "yellow",
    });
    run();
    expect(onStart.mock.calls[0][0]).toMatchObject({
      via_gateway: true,
      sensitivity: "yellow",
    });
  });

  it("toasts and aborts on invalid worker_provenance JSON", () => {
    const { onStart, toast, setSubmitting, run } = submit({
      options: options({ workerProvJson: "{not-json" }),
    });
    run();
    expect(toast).toHaveBeenCalledWith("error", "worker_provenance must be valid JSON");
    expect(onStart).not.toHaveBeenCalled();
    expect(setSubmitting).not.toHaveBeenCalled();
  });

  it("parses worker_provenance and includes scope_auditor when requested", () => {
    const { onStart, run } = submit({
      options: options({
        workerProvJson: '{"runner":"test"}',
        scopeAuditor: true,
      }),
    });
    run();
    const payload = onStart.mock.calls[0][0] as StartPayload & {
      scope_auditor?: boolean;
    };
    expect(payload.worker_provenance).toEqual({ runner: "test" });
    expect(payload.scope_auditor).toBe(true);
  });
});
