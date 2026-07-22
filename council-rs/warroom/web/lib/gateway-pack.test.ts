import { describe, expect, it } from "vitest";
import {
  canEnableGovernedProceeding,
  gatewayPackIsCoreNeutral,
  gatewayPackStateLabel,
  type GatewayPackStatus,
} from "./gateway-pack";

function status(
  partial: Partial<GatewayPackStatus> & Pick<GatewayPackStatus, "state">,
): GatewayPackStatus {
  return {
    message: "",
    pack_version: null,
    manifest_mode: null,
    gateway_url: "http://127.0.0.1:18080",
    project: "irin-desktop-gateway",
    key_id: null,
    enabled: false,
    docker: "ready",
    watch_producer_enabled: false,
    watch_dispatcher_enabled: false,
    authenticated: false,
    support_matrix_summary: "",
    ...partial,
  };
}

describe("gateway pack state labels", () => {
  it("never labels a bare URL state as ready", () => {
    expect(gatewayPackStateLabel("not_installed")).not.toMatch(/ready/i);
    expect(gatewayPackStateLabel("installed_stopped")).not.toMatch(/ready/i);
    expect(gatewayPackStateLabel("authenticated_ready")).toMatch(/Authenticated ready/);
  });
});

describe("core-neutral states", () => {
  it("treats missing Docker as non-red for core", () => {
    expect(gatewayPackIsCoreNeutral("docker_missing")).toBe(true);
    expect(gatewayPackIsCoreNeutral("docker_daemon_down")).toBe(true);
    expect(gatewayPackIsCoreNeutral("degraded")).toBe(false);
  });
});

describe("governed proceeding gate", () => {
  it("blocks governed on installed-release until authenticated ready", () => {
    const stopped = status({ state: "installed_stopped" });
    expect(
      canEnableGovernedProceeding(stopped, {
        requireInstalledRelease: true,
        desktopMode: "installed-release",
      }),
    ).toBe(false);

    const ready = status({
      state: "authenticated_ready",
      authenticated: true,
      enabled: true,
    });
    expect(
      canEnableGovernedProceeding(ready, {
        requireInstalledRelease: true,
        desktopMode: "installed-release",
      }),
    ).toBe(true);
  });

  it("requires authenticated flag even if state string is ready", () => {
    const fake = status({
      state: "authenticated_ready",
      authenticated: false,
    });
    expect(canEnableGovernedProceeding(fake)).toBe(false);
  });

  it("allows development mode without pack", () => {
    expect(
      canEnableGovernedProceeding(null, { desktopMode: "development" }),
    ).toBe(true);
  });
});
