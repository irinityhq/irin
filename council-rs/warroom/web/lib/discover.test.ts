import { describe, expect, it } from "vitest";
import { normalizeDiscoverResponse, providerModelMap } from "./discover";

describe("normalizeDiscoverResponse", () => {
  it("passes a well-formed contract payload through", () => {
    const raw = {
      providers: [
        {
          name: "Grok (xAI)",
          available: true,
          source: "env",
          env_hint: "XAI_API_KEY",
          models: ["grok-4.3"],
        },
      ],
      log: ["✅ Grok (xAI) — XAI_API_KEY detected"],
    };
    expect(normalizeDiscoverResponse(raw)).toEqual({
      ...raw,
      providers: [
        {
          ...raw.providers[0],
          label: "Grok (xAI)",
          family: "",
          transport: "",
          gateway_supported: false,
        },
      ],
    });
  });

  it("coerces empty env_hint to null", () => {
    const r = normalizeDiscoverResponse({
      providers: [
        { name: "Vertex", available: true, source: "adc", env_hint: "", models: [] },
      ],
      log: [],
    });
    expect(r.providers[0].env_hint).toBeNull();
  });

  it("rejects env hints that are not safe environment-variable names", () => {
    const r = normalizeDiscoverResponse({
      providers: [
        {
          name: "custom",
          available: false,
          source: "userconfig",
          env_hint: "not an env name/or a value",
          models: [],
        },
      ],
      log: [],
    });
    expect(r.providers[0].env_hint).toBeNull();
  });

  it("defaults missing/malformed fields safely", () => {
    const r = normalizeDiscoverResponse({
      providers: [{ name: "Nous" }, { name: "" }, "junk", null],
      log: ["ok", 42, null],
    });
    expect(r.providers).toEqual([
      {
        name: "Nous",
        label: "Nous",
        family: "",
        transport: "",
        available: false,
        gateway_supported: false,
        source: "",
        env_hint: null,
        models: [],
      },
    ]);
    expect(r.log).toEqual(["ok"]);
  });

  it("drops non-string and empty model entries", () => {
    const r = normalizeDiscoverResponse({
      providers: [
        {
          name: "DeepSeek",
          available: true,
          source: "env",
          env_hint: "DEEPSEEK_API_KEY",
          models: ["deepseek-chat", "", 7, null],
        },
      ],
      log: [],
    });
    expect(r.providers[0].models).toEqual(["deepseek-chat"]);
  });

  it("sorts available providers first, then by name", () => {
    const r = normalizeDiscoverResponse({
      providers: [
        { name: "Zeta", available: false, source: "", env_hint: "ZETA_API_KEY", models: [] },
        { name: "Beta", available: true, source: "env", env_hint: null, models: [] },
        { name: "Alpha", available: false, source: "", env_hint: "ALPHA_API_KEY", models: [] },
        { name: "Echo", available: true, source: "env", env_hint: null, models: [] },
      ],
      log: [],
    });
    expect(r.providers.map((p) => p.name)).toEqual([
      "Beta",
      "Echo",
      "Alpha",
      "Zeta",
    ]);
  });

  it("returns an empty shape for garbage input", () => {
    expect(normalizeDiscoverResponse(undefined)).toEqual({ providers: [], log: [] });
    expect(normalizeDiscoverResponse("nope")).toEqual({ providers: [], log: [] });
    expect(normalizeDiscoverResponse({})).toEqual({ providers: [], log: [] });
  });
});

describe("providerModelMap", () => {
  it("builds name -> models from discover response", () => {
    const data = {
      providers: [
        { name: "grok_api", label: "Grok API", family: "xai", transport: "api", available: true, source: "env", env_hint: "XAI_API_KEY", models: ["grok-4.3"] },
        { name: "grok_build", label: "Grok Build", family: "xai", transport: "cli", available: false, source: "cli", env_hint: null, models: ["grok-build"] },
        { name: "ollama", label: "Ollama", family: "local", transport: "local", available: true, source: "local", env_hint: null, models: ["llama3", "mistral"] },
      ],
      log: [],
    };
    expect(providerModelMap(data)).toEqual({
      grok_api: ["grok-4.3"],
      ollama: ["llama3", "mistral"],
    });
  });

  it("skips providers without models", () => {
    const data = {
      providers: [{ name: "empty", label: "empty", family: "", transport: "", available: true, source: "", env_hint: null, models: [] }],
      log: [],
    };
    expect(providerModelMap(data)).toEqual({});
  });
});

import {
  buildProviderChoices,
  getModelsForProvider,
  normalizeProviderKey,
  unsupportedGatewayTransportReason,
  unavailableProviderReason,
} from "./use-discover";

describe("normalizeProviderKey + getModelsForProvider", () => {
  const map = {
    grok_api: ["grok-4.3"],
    grok_build: ["grok-build"],
    grok_hermes: ["grok-4.20"],
  };

  it("normalizes casing without collapsing transport identities", () => {
    expect(normalizeProviderKey(" GROK_API ")).toBe("grok_api");
    expect(normalizeProviderKey("grok_build")).toBe("grok_build");
    expect(normalizeProviderKey("grok_hermes")).toBe("grok_hermes");
    expect(normalizeProviderKey("grok_cli")).toBe("grok_cli");
  });

  it("resolves models for the exact transport only", () => {
    expect(getModelsForProvider(map, "grok_api")).toEqual(["grok-4.3"]);
    expect(getModelsForProvider(map, "GROK_BUILD")).toEqual(["grok-build"]);
    expect(getModelsForProvider(map, "grok_hermes")).toEqual(["grok-4.20"]);
    expect(getModelsForProvider(map, "grok_cli")).toEqual([]);
  });

  it("falls back to empty for unknown", () => {
    expect(getModelsForProvider(map, "foo_cli")).toEqual([]);
  });
});

describe("unsupportedGatewayTransportReason", () => {
  const providers = normalizeDiscoverResponse({
    providers: [
      { name: "grok_api", available: true, gateway_supported: true },
      { name: "grok_hermes", available: true, gateway_supported: false },
    ],
  }).providers;

  it("blocks an available direct-only transport in Governed mode", () => {
    expect(unsupportedGatewayTransportReason(providers, ["grok_api"])).toBeNull();
    expect(unsupportedGatewayTransportReason(providers, ["grok_hermes"])).toContain(
      "grok_hermes",
    );
  });
});

describe("unavailableProviderReason", () => {
  const providers = normalizeDiscoverResponse({
    providers: [
      { name: "grok_api", label: "Grok API", family: "xai", transport: "api", available: true, models: [] },
      { name: "grok_build", label: "Grok Build", family: "xai", transport: "cli", available: false, models: [] },
    ],
  }).providers;

  it("allows only an explicitly available transport", () => {
    expect(unavailableProviderReason(providers, ["grok_api"])).toBeNull();
    expect(unavailableProviderReason(providers, ["grok_build"])).toContain("grok_build");
  });

  it("treats an unknown legacy ID as unavailable instead of aliasing it", () => {
    expect(unavailableProviderReason(providers, ["grok_cli"])).toContain("grok_cli");
  });

  it("keeps every transport visible while disabling unavailable and legacy choices", () => {
    expect(buildProviderChoices(providers, "grok_cli")).toEqual([
      {
        name: "grok_cli",
        label: "grok_cli — legacy/unavailable",
        available: false,
        legacy: true,
      },
      {
        name: "grok_api",
        label: "Grok API (grok_api)",
        available: true,
        legacy: false,
      },
      {
        name: "grok_build",
        label: "Grok Build (grok_build) — unavailable",
        available: false,
        legacy: false,
      },
    ]);
  });
});
