import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  canEnableGovernedProceeding,
  gatewayHeaderTruth,
  gatewayPackIsCoreNeutral,
  gatewayPackStateLabel,
  type GatewayPackStatus,
} from "./gateway-pack";
import { gatewayPackAllowsGoverned } from "./tauri";

function status(
  partial: Partial<GatewayPackStatus> & Pick<GatewayPackStatus, "state">,
): GatewayPackStatus {
  const authenticated = partial.authenticated ?? false;
  const enabled = partial.enabled ?? false;
  const council_governed = partial.council_governed ?? false;
  const governed_ready =
    partial.governed_ready ??
    (partial.state === "authenticated_ready" && authenticated && council_governed);
  return {
    message: "",
    pack_version: null,
    manifest_mode: null,
    gateway_url: "http://127.0.0.1:18080",
    project: "irin-desktop-gateway",
    key_id: null,
    enabled,
    docker: "ready",
    watch_producer_enabled: false,
    watch_dispatcher_enabled: false,
    authenticated,
    council_governed,
    gateway_url_configured: true,
    support_matrix_summary: "",
    spawn_capable: partial.spawn_capable ?? (enabled && authenticated),
    governed_ready,
    hard_down:
      partial.hard_down ??
      (!enabled ||
        partial.state === "docker_missing" ||
        partial.state === "docker_daemon_down" ||
        partial.state === "not_installed" ||
        partial.state === "installed_stopped" ||
        partial.state === "disabled"),
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
      council_governed: true,
      governed_ready: true,
    });
    expect(
      canEnableGovernedProceeding(ready, {
        requireInstalledRelease: true,
        desktopMode: "installed-release",
      }),
    ).toBe(true);
  });

  it("reads governed_ready only — does not re-derive from state fields", () => {
    const fake = status({
      state: "authenticated_ready",
      authenticated: true,
      council_governed: true,
      governed_ready: false,
    });
    expect(canEnableGovernedProceeding(fake)).toBe(false);

    const onlyField = status({
      state: "degraded",
      authenticated: false,
      council_governed: false,
      governed_ready: true,
    });
    expect(canEnableGovernedProceeding(onlyField)).toBe(true);
  });

  it("allows development mode without pack", () => {
    expect(
      canEnableGovernedProceeding(null, { desktopMode: "development" }),
    ).toBe(true);
  });

  it("fails closed while the desktop build mode is still detecting", () => {
    const ready = status({
      state: "authenticated_ready",
      authenticated: true,
      enabled: true,
      council_governed: true,
    });
    expect(
      canEnableGovernedProceeding(ready, {
        requireInstalledRelease: true,
        desktopMode: "detecting",
      }),
    ).toBe(false);
    expect(
      canEnableGovernedProceeding(null, {
        requireInstalledRelease: true,
        desktopMode: "detecting",
      }),
    ).toBe(false);
  });

  it("fails closed when the desktop build mode is unavailable", () => {
    expect(
      canEnableGovernedProceeding(null, {
        requireInstalledRelease: true,
        desktopMode: "unavailable",
      }),
    ).toBe(false);
  });

  it("keeps the browser path free while the pack status is unknown", () => {
    expect(
      canEnableGovernedProceeding(null, {
        requireInstalledRelease: true,
        desktopMode: "development",
      }),
    ).toBe(true);
  });
});

describe("gatewayPackAllowsGoverned vocabulary", () => {
  it("is a single-field read of governed_ready", () => {
    expect(
      gatewayPackAllowsGoverned(
        status({
          state: "degraded",
          governed_ready: true,
        }),
      ),
    ).toBe(true);
    expect(
      gatewayPackAllowsGoverned(
        status({
          state: "authenticated_ready",
          authenticated: true,
          council_governed: true,
          governed_ready: false,
        }),
      ),
    ).toBe(false);
    // Source shape guard: implementation must not re-derive.
    const source = readFileSync(path.join(__dirname, "tauri.ts"), "utf8");
    expect(source).toMatch(
      /export function gatewayPackAllowsGoverned[\s\S]*?return status\?\.governed_ready === true;/,
    );
  });
});

describe("gateway header truth", () => {
  it("distinguishes url-set from pack-authenticated and governed", () => {
    expect(gatewayHeaderTruth(null, true).label).toBe("url set");
    expect(gatewayHeaderTruth(null, false).label).toBe("not set");
    expect(
      gatewayHeaderTruth(
        status({
          state: "authenticated_ready",
          authenticated: true,
          enabled: true,
          council_governed: true,
        }),
        true,
      ).label,
    ).toBe("governed");
    expect(
      gatewayHeaderTruth(
        status({
          state: "authenticated_ready",
          authenticated: true,
          enabled: true,
          council_governed: false,
        }),
        true,
      ).label,
    ).toBe("pack auth");
    expect(
      gatewayHeaderTruth(status({ state: "docker_missing" }), false).tone,
    ).toBe("neutral");
  });
});

describe("status authority migration", () => {
  it("no longer exports renderer operation fences", () => {
    const source = readFileSync(path.join(__dirname, "gateway-pack.ts"), "utf8");
    expect(source).not.toContain("GatewayPackOperationFence");
    expect(source).not.toContain("shouldApplyBackgroundStatusPoll");
    expect(source).not.toContain("runGatewayPackStatusWriterIfCurrent");
  });
});
