import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  beginGatewayPackAction,
  canEnableGovernedProceeding,
  createGatewayPackOperationFence,
  endGatewayPackAction,
  gatewayPackIsCoreNeutral,
  gatewayPackStateLabel,
  runGatewayPackStatusWriterIfCurrent,
  shouldApplyBackgroundStatusPoll,
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

describe("gateway pack operation fence", () => {
  it("rejects a status read that started before a lifecycle action", async () => {
    const fence = createGatewayPackOperationFence();
    const degraded = status({ state: "degraded", message: "stale" });
    let resolveRead!: (value: GatewayPackStatus) => void;
    const read = new Promise<GatewayPackStatus>((resolve) => {
      resolveRead = resolve;
    });
    const applied: GatewayPackStatus[] = [];
    const completion = runGatewayPackStatusWriterIfCurrent(
      fence,
      () => read,
      (value) => applied.push(value),
      () => {},
    );

    beginGatewayPackAction(fence);
    resolveRead(degraded);

    expect(await completion).toBe("stale");
    expect(applied).toEqual([]);
  });

  it("does not start a new poll while a lifecycle action is running", async () => {
    const fence = createGatewayPackOperationFence();
    let readCalled = false;
    beginGatewayPackAction(fence);

    const outcome = await runGatewayPackStatusWriterIfCurrent(
      fence,
      async () => {
        readCalled = true;
        return status({ state: "degraded" });
      },
      () => {},
      () => {},
    );

    expect(outcome).toBe("blocked");
    expect(readCalled).toBe(false);
  });

  it("applies only a current post-action success or error", async () => {
    const fence = createGatewayPackOperationFence();
    beginGatewayPackAction(fence);
    endGatewayPackAction(fence);
    const ready = status({
      state: "authenticated_ready",
      authenticated: true,
      enabled: true,
      council_governed: true,
    });
    let applied: GatewayPackStatus | null = null;
    expect(
      await runGatewayPackStatusWriterIfCurrent(
        fence,
        async () => ready,
        (value) => {
          applied = value;
        },
        () => {},
      ),
    ).toBe("applied");
    expect(applied).toBe(ready);

    let errored = false;
    expect(
      await runGatewayPackStatusWriterIfCurrent(
        fence,
        async () => {
          throw new Error("status failed");
        },
        () => {},
        () => {
          errored = true;
        },
      ),
    ).toBe("applied");
    expect(errored).toBe(true);
  });

  it("keeps last projection when a poll errors (caller no-ops onError)", async () => {
    const fence = createGatewayPackOperationFence();
    const prior = status({
      state: "authenticated_ready",
      authenticated: true,
      enabled: true,
      council_governed: true,
      governed_ready: true,
    });
    let visible: GatewayPackStatus | null = prior;
    const outcome = await runGatewayPackStatusWriterIfCurrent(
      fence,
      async () => {
        throw new Error("transient");
      },
      (next) => {
        visible = next;
      },
      () => {
        // keep-last: do not null
      },
    );
    expect(outcome).toBe("applied");
    expect(visible).toEqual(prior);
  });

  it("defers overlapping polls: only the latest epoch may write", async () => {
    expect(shouldApplyBackgroundStatusPoll(3, 3, false)).toBe(true);
    expect(shouldApplyBackgroundStatusPoll(2, 3, false)).toBe(false);
    expect(shouldApplyBackgroundStatusPoll(3, 3, true)).toBe(false);

    const fence = createGatewayPackOperationFence();
    let resolveSlow!: (value: GatewayPackStatus) => void;
    const slow = new Promise<GatewayPackStatus>((resolve) => {
      resolveSlow = resolve;
    });
    const applied: GatewayPackStatus[] = [];
    let epoch = 0;

    const firstEpoch = ++epoch;
    const first = runGatewayPackStatusWriterIfCurrent(
      fence,
      () => slow,
      (next) => {
        if (!shouldApplyBackgroundStatusPoll(firstEpoch, epoch, false)) return;
        applied.push(next);
      },
      () => {},
    );

    const secondEpoch = ++epoch;
    const secondStatus = status({
      state: "degraded",
      enabled: true,
      authenticated: true,
    });
    const second = runGatewayPackStatusWriterIfCurrent(
      fence,
      async () => secondStatus,
      (next) => {
        if (!shouldApplyBackgroundStatusPoll(secondEpoch, epoch, false)) return;
        applied.push(next);
      },
      () => {},
    );

    expect(await second).toBe("applied");
    resolveSlow(
      status({
        state: "authenticated_ready",
        authenticated: true,
        enabled: true,
        council_governed: true,
        governed_ready: true,
      }),
    );
    expect(await first).toBe("applied");
    // Slow older epoch must not write after the newer poll settled.
    expect(applied).toEqual([secondStatus]);
  });

  it("action fence stales a poll begun before disarm/action", async () => {
    const fence = createGatewayPackOperationFence();
    let resolveRead!: (value: GatewayPackStatus) => void;
    const read = new Promise<GatewayPackStatus>((resolve) => {
      resolveRead = resolve;
    });
    let visible: GatewayPackStatus | null = status({
      state: "authenticated_ready",
      authenticated: true,
      enabled: true,
      council_governed: true,
      governed_ready: true,
    });
    const completion = runGatewayPackStatusWriterIfCurrent(
      fence,
      () => read,
      (next) => {
        visible = next;
      },
      () => {
        visible = null;
      },
    );
    beginGatewayPackAction(fence);
    resolveRead(status({ state: "degraded", message: "stale" }));
    expect(await completion).toBe("stale");
    expect(visible?.state).toBe("authenticated_ready");
  });

  it("phone-style deferred poll is blocked by epoch invalidation and actionBusy", async () => {
    // Mirrors SettingsPanel phone Enable/Disable fencing: bump epoch + set
    // actionBusy at the action boundary so a slow background poll cannot write.
    type PhoneLike = { state: string; message: string };
    let resolvePoll!: (value: PhoneLike) => void;
    const poll = new Promise<PhoneLike>((resolve) => {
      resolvePoll = resolve;
    });
    let epoch = 0;
    let actionBusy = false;
    const prior: PhoneLike = { state: "ready", message: "published" };
    let visible: PhoneLike = prior;

    const pollEpoch = ++epoch;
    const completion = (async () => {
      const next = await poll;
      if (!shouldApplyBackgroundStatusPoll(pollEpoch, epoch, actionBusy)) {
        return "blocked" as const;
      }
      visible = next;
      return "applied" as const;
    })();

    // Enable/Disable starts while the poll is still deferred.
    actionBusy = true;
    epoch += 1;
    resolvePoll({ state: "off", message: "stale background sample" });

    expect(await completion).toBe("blocked");
    expect(visible).toEqual(prior);
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

import { gatewayHeaderTruth } from "./gateway-pack";

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
