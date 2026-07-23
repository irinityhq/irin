import { describe, expect, it } from "vitest";
import {
  councilPortFromApiBase,
  defaultsForPage,
  dropRemoteLoopbackOverrides,
  mergeConfigSources,
  mergeNativeAndLocalConfig,
  pickConfigValue,
  pickOptionalConfigValue,
} from "./runtime-config";

describe("runtime-config merge", () => {
  const defaults = {
    apiBase: "http://127.0.0.1:8765",
    wsBase: "ws://127.0.0.1:8765",
    gatewayBase: "http://127.0.0.1:18080",
    authToken: "",
    councilPath: "",
    councilRoot: "",
    librarianBase: "http://127.0.0.1:11435",
  };

  it("pickConfigValue treats empty string as unset", () => {
    expect(pickConfigValue("", defaults.apiBase)).toBe(defaults.apiBase);
    expect(pickConfigValue("  ", defaults.apiBase)).toBe(defaults.apiBase);
    expect(pickConfigValue(" http://x ", defaults.apiBase)).toBe("http://x");
  });

  it("pickOptionalConfigValue preserves empty string as disabled", () => {
    expect(pickOptionalConfigValue(undefined, defaults.gatewayBase)).toBe(
      defaults.gatewayBase,
    );
    expect(pickOptionalConfigValue("", defaults.gatewayBase)).toBe("");
    expect(pickOptionalConfigValue("  ", defaults.gatewayBase)).toBe("");
    expect(pickOptionalConfigValue(" http://x ", defaults.gatewayBase)).toBe(
      "http://x",
    );
  });

  it("mergeConfigSources applies trimmed overrides", () => {
    expect(
      mergeConfigSources({ gatewayBase: " http://127.0.0.1:8080 " }, defaults)
        .gatewayBase,
    ).toBe("http://127.0.0.1:8080");
  });

  it("keeps desktop-selected endpoints authoritative over saved browser settings", () => {
    expect(
      mergeNativeAndLocalConfig(
        {
          apiBase: "http://127.0.0.1:20321",
          wsBase: "ws://127.0.0.1:20321",
        },
        {
          apiBase: "http://127.0.0.1:8765",
          wsBase: "ws://127.0.0.1:8765",
        },
      ),
    ).toMatchObject({
      apiBase: "http://127.0.0.1:20321",
      wsBase: "ws://127.0.0.1:20321",
    });
    expect(
      mergeNativeAndLocalConfig(
        { apiBase: "http://127.0.0.1:20321" },
        {},
      ),
    ).toMatchObject({ apiBase: "http://127.0.0.1:20321" });
    expect(
      mergeNativeAndLocalConfig(
        {
          apiBase: "http://127.0.0.1:20321",
          wsBase: "ws://127.0.0.1:20321",
        },
        { apiBase: "http://127.0.0.1:8765", wsBase: "ws://127.0.0.1:8765" },
      ),
    ).toMatchObject({
      apiBase: "http://127.0.0.1:20321",
      wsBase: "ws://127.0.0.1:20321",
    });
  });

  it("keeps browser-saved endpoints configurable when no native config exists", () => {
    expect(
      mergeNativeAndLocalConfig(
        {},
        {
          apiBase: "http://127.0.0.1:20321",
          wsBase: "ws://127.0.0.1:20321",
        },
      ),
    ).toMatchObject({
      apiBase: "http://127.0.0.1:20321",
      wsBase: "ws://127.0.0.1:20321",
    });
  });

  it("mergeConfigSources lets optional gatewayBase be cleared", () => {
    expect(mergeConfigSources({ gatewayBase: "" }, defaults).gatewayBase).toBe(
      "",
    );
  });

  it("merges councilRoot (feature contract) and defaults it empty", () => {
    expect(mergeConfigSources({}, defaults).councilRoot).toBe("");
    expect(
      mergeConfigSources({ councilRoot: " /tmp/council-base " }, defaults)
        .councilRoot,
    ).toBe("/tmp/council-base");
  });

  it("uses one origin when the browser is served remotely", () => {
    expect(defaultsForPage(defaults, "https://warroom.example.test/")).toEqual({
      ...defaults,
      apiBase: "https://warroom.example.test",
      wsBase: "wss://warroom.example.test",
      gatewayBase: "https://warroom.example.test",
    });
  });

  it("keeps loopback defaults for local browser and desktop lanes", () => {
    expect(defaultsForPage(defaults, "http://127.0.0.1:3010/")).toEqual(
      defaults,
    );
    expect(defaultsForPage(defaults, undefined)).toEqual(defaults);
  });

  it("drops stale loopback device overrides on a remote origin", () => {
    const remote = defaultsForPage(defaults, "https://warroom.example.test/");
    expect(
      dropRemoteLoopbackOverrides(
        {
          apiBase: "http://127.0.0.1:8766",
          wsBase: "ws://localhost:8766",
          gatewayBase: "http://127.0.0.1:18080",
          authToken: "kept",
        },
        remote,
      ),
    ).toEqual({ authToken: "kept" });
  });

  it("derives the isolated native Council port from loopback API config", () => {
    expect(councilPortFromApiBase("http://127.0.0.1:20321")).toBe(20321);
    expect(councilPortFromApiBase("http://localhost:8765")).toBe(8765);
    expect(councilPortFromApiBase("https://example.test")).toBe(8765);
    expect(councilPortFromApiBase("not-a-url")).toBe(8765);
  });
});
