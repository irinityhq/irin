/**
 * Behavioral contracts for War Room runtime reconciliation:
 * config save emit, boot-health re-arm policy, Pack→probe→retry→truth
 * driven through the production startWarRoomBackendReady effect body.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { reconcileBootHealthAfterConfigChange } from "./boot-health-poll";
import {
  emitWarroomConfigChanged,
  saveRuntimeConfig,
  subscribeWarroomConfigChanged,
  WARROOM_CONFIG_CHANGED_EVENT,
} from "./runtime-config";
import { runGatewayPackActionOnce } from "../components/settings/useDesktopActions";
import { warroomHealthLabel, COUNCIL_LOADING_LABEL } from "./warroom-health-label";
import { gatewayHeaderTruth } from "./gateway-pack";
import { startWarRoomBackendReady } from "./warroom-backend-ready";
import type {
  DesktopStatusSnapshot,
  GatewayPackStatus,
  PhoneAccessStatus,
  TouchIdStatus,
} from "./tauri";

function installEventTargetWindow() {
  const target = new EventTarget();
  const durable = new Map<string, string>();
  const session = new Map<string, string>();
  const localStorage = {
    getItem: (k: string) => durable.get(k) ?? null,
    setItem: (k: string, v: string) => {
      durable.set(k, v);
    },
    removeItem: (k: string) => {
      durable.delete(k);
    },
  };
  const sessionStorage = {
    getItem: (k: string) => session.get(k) ?? null,
    setItem: (k: string, v: string) => {
      session.set(k, v);
    },
    removeItem: (k: string) => {
      session.delete(k);
    },
  };
  const w = {
    dispatchEvent: (ev: Event) => target.dispatchEvent(ev),
    addEventListener: (
      type: string,
      listener: EventListenerOrEventListenerObject,
      options?: boolean | AddEventListenerOptions,
    ) => target.addEventListener(type, listener, options),
    removeEventListener: (
      type: string,
      listener: EventListenerOrEventListenerObject,
      options?: boolean | EventListenerOptions,
    ) => target.removeEventListener(type, listener, options),
    localStorage,
    sessionStorage,
    location: { href: "http://127.0.0.1:3010/" },
  };
  // runtime-config uses bare localStorage/sessionStorage globals, not only window.*.
  vi.stubGlobal("window", w);
  vi.stubGlobal("localStorage", localStorage);
  vi.stubGlobal("sessionStorage", sessionStorage);
  return w;
}

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
}

function pack(over: Partial<GatewayPackStatus> = {}): GatewayPackStatus {
  return {
    state: "authenticated_ready",
    message: "ready",
    pack_version: "1",
    manifest_mode: "local-dev",
    gateway_url: "http://127.0.0.1:18080",
    project: "irin-desktop-gateway",
    key_id: "k1",
    enabled: true,
    docker: "ready",
    watch_producer_enabled: false,
    watch_dispatcher_enabled: false,
    authenticated: true,
    council_governed: true,
    gateway_url_configured: true,
    support_matrix_summary: "",
    spawn_capable: true,
    governed_ready: true,
    hard_down: false,
    ...over,
  };
}

function touch(over: Partial<TouchIdStatus> = {}): TouchIdStatus {
  return {
    state: "ready",
    reason: null,
    armed_exp_at_ms: null,
    armed_expires_in_ms: null,
    stage_expires_in_ms: null,
    enrolled: true,
    allow_real_arm: true,
    can_enroll: false,
    can_arm: true,
    can_renew: false,
    can_disarm: false,
    rehearsal_passed: false,
    ...over,
  };
}

function phone(over: Partial<PhoneAccessStatus> = {}): PhoneAccessStatus {
  return {
    state: "off",
    message: "off",
    tailnet_url: null,
    enabled: false,
    ownership: "none",
    interrupted: false,
    gateway_routes: false,
    funnel_present: false,
    ...over,
  };
}

function snap(over: Partial<DesktopStatusSnapshot> = {}): DesktopStatusSnapshot {
  return {
    authority_epoch: "epoch-a",
    seq: 1,
    pack: pack(),
    touch_id: touch(),
    phone: phone(),
    ...over,
  };
}

describe("runtime config change signal", () => {
  beforeEach(() => {
    installEventTargetWindow();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("saveRuntimeConfig emits exactly one config-change event", async () => {
    const seen: string[] = [];
    const unsub = subscribeWarroomConfigChanged(() => {
      seen.push("changed");
    });
    await saveRuntimeConfig({ gatewayBase: "http://127.0.0.1:18081" });
    expect(seen).toEqual(["changed"]);
    unsub();
  });

  it("emit + subscribe use the shared event name", () => {
    const handler = vi.fn();
    const unsub = subscribeWarroomConfigChanged(handler);
    emitWarroomConfigChanged();
    expect(handler).toHaveBeenCalledTimes(1);
    // A foreign event must not fire the helper.
    window.dispatchEvent(new Event("unrelated-event"));
    expect(handler).toHaveBeenCalledTimes(1);
    unsub();
    emitWarroomConfigChanged();
    expect(handler).toHaveBeenCalledTimes(1);
    expect(WARROOM_CONFIG_CHANGED_EVENT).toBe("warroom-config-changed");
  });
});

describe("reconcileBootHealthAfterConfigChange policy", () => {
  it("ready marks online (no force re-arm)", () => {
    const markOnline = vi.fn();
    const startConnecting = vi.fn();
    reconcileBootHealthAfterConfigChange(
      { markOnline, startConnecting },
      true,
      { forceRearmOnFailure: true },
    );
    expect(markOnline).toHaveBeenCalledTimes(1);
    expect(startConnecting).not.toHaveBeenCalled();
  });

  it("Tauri failure force-rearms from online", () => {
    const markOnline = vi.fn();
    const startConnecting = vi.fn();
    reconcileBootHealthAfterConfigChange(
      { markOnline, startConnecting },
      false,
      { forceRearmOnFailure: true },
    );
    expect(markOnline).not.toHaveBeenCalled();
    expect(startConnecting).toHaveBeenCalledTimes(1);
    expect(startConnecting).toHaveBeenCalledWith({ force: true });
  });

  it("hosted failure does not force-rearm", () => {
    const markOnline = vi.fn();
    const startConnecting = vi.fn();
    reconcileBootHealthAfterConfigChange(
      { markOnline, startConnecting },
      false,
      { forceRearmOnFailure: false },
    );
    expect(markOnline).not.toHaveBeenCalled();
    expect(startConnecting).not.toHaveBeenCalled();
  });
});

describe("production startWarRoomBackendReady — Pack → event → probe → retry → truth", () => {
  beforeEach(() => {
    installEventTargetWindow();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("drives config-change through the WarRoom effect body (Tauri force + retry)", async () => {
    const timers: Array<() => void> = [];
    const setTimeoutFn = ((callback: () => void) => {
      timers.push(callback);
      return timers.length;
    }) as unknown as typeof setTimeout;

    let bootRetryActive = false;
    let gatewayPack: GatewayPackStatus | null = pack({
      state: "disabled",
      authenticated: false,
      enabled: false,
      council_governed: false,
      governed_ready: false,
      hard_down: true,
    });

    // loadInitialState sequence:
    // 1) mount probe → ready (online)
    // 2) config-change probe → not_ready (Tauri force re-arm)
    // 3) poller connecting probe → not_ready (schedules retry)
    // 4) retry probe → ready (online)
    const loadInitialState = vi
      .fn<() => Promise<boolean>>()
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true);

    const startCouncilServer = vi.fn(async () => undefined);

    const handle = startWarRoomBackendReady({
      loadInitialState,
      isTauri: () => true,
      // Leave startup branch pending so this test isolates config-change re-arm.
      nativeOwnsCouncilStartup: () => new Promise(() => undefined),
      startCouncilServer,
      getConfigForStartup: () => new Promise(() => undefined),
      initRuntimeConfig: () => undefined,
      onRetryActiveChange: (active) => {
        bootRetryActive = active;
      },
      pollerOptions: {
        baseDelayMs: 1,
        maxDelayMs: 1,
        connectingBudgetMs: 100,
        recoveryIntervalMs: 1,
        now: () => 0,
        setTimeoutFn,
        clearTimeoutFn: () => undefined,
      },
    });

    await flushMicrotasks();
    // Mount ready → online (startup ownership branch intentionally not resolved).
    expect(handle.poller().phase()).toBe("online");
    expect(startCouncilServer).not.toHaveBeenCalled();

    // Pack success emits the production config-change event; only the
    // startWarRoomBackendReady subscription may re-arm (no test-side policy).
    const packSnap = snap({
      seq: 3,
      pack: pack({
        state: "authenticated_ready",
        authenticated: true,
        enabled: true,
        council_governed: true,
        governed_ready: true,
        hard_down: false,
      }),
    });
    await runGatewayPackActionOnce(
      async () => packSnap,
      (next) => {
        gatewayPack = next.pack;
      },
      () => undefined,
      () => undefined,
      () => emitWarroomConfigChanged(),
    );

    await flushMicrotasks();
    // Config-change loadInitialState returned false → Tauri force re-arm → connecting.
    expect(handle.poller().phase()).toBe("connecting");
    expect(bootRetryActive).toBe(true);
    expect(
      warroomHealthLabel(null, null, "error", bootRetryActive),
    ).toBe(COUNCIL_LOADING_LABEL);

    // Connecting probe returned not_ready → scheduled retry.
    await flushMicrotasks();
    timers.shift()?.();
    await flushMicrotasks();
    expect(handle.poller().phase()).toBe("online");
    expect(bootRetryActive).toBe(false);
    expect(
      warroomHealthLabel("1.0.0", "1", "online", bootRetryActive),
    ).toBe("gen 1.0.0 · stream 1");
    expect(gatewayHeaderTruth(gatewayPack, true).label).toBe("governed");
    // Production path alone: at least mount + config-change + connecting probes.
    expect(loadInitialState.mock.calls.length).toBeGreaterThanOrEqual(3);

    handle.stop();
  });

  it("hosted config-change failure does not force-rearm via production policy", async () => {
    const timers: Array<() => void> = [];
    const setTimeoutFn = ((callback: () => void) => {
      timers.push(callback);
      return timers.length;
    }) as unknown as typeof setTimeout;

    const loadInitialState = vi
      .fn<() => Promise<boolean>>()
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(false);

    const handle = startWarRoomBackendReady({
      loadInitialState,
      isTauri: () => false,
      nativeOwnsCouncilStartup: async () => false,
      startCouncilServer: async () => undefined,
      getConfigForStartup: async () => ({
        apiBase: "http://127.0.0.1:8765",
        authToken: "",
        librarianBase: "",
      }),
      initRuntimeConfig: () => undefined,
      pollerOptions: {
        baseDelayMs: 1,
        maxDelayMs: 1,
        connectingBudgetMs: 100,
        recoveryIntervalMs: 1,
        now: () => 0,
        setTimeoutFn,
        clearTimeoutFn: () => undefined,
      },
    });

    await flushMicrotasks();
    expect(handle.poller().phase()).toBe("online");

    emitWarroomConfigChanged();
    await flushMicrotasks();
    // Hosted: not_ready must not force re-arm from online.
    expect(handle.poller().phase()).toBe("online");
    expect(timers).toHaveLength(0);

    handle.stop();
  });
});

describe("production startWarRoomBackendReady — packaged cold-launch ownership", () => {
  beforeEach(() => {
    installEventTargetWindow();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("native ownership must not call startCouncilServer", async () => {
    const timers: Array<() => void> = [];
    const setTimeoutFn = ((callback: () => void) => {
      timers.push(callback);
      return timers.length;
    }) as unknown as typeof setTimeout;

    const startCouncilServer = vi.fn(async () => undefined);
    const loadInitialState = vi.fn(async () => false);

    const handle = startWarRoomBackendReady({
      loadInitialState,
      isTauri: () => true,
      nativeOwnsCouncilStartup: async () => true,
      startCouncilServer,
      getConfigForStartup: async () => ({
        apiBase: "http://127.0.0.1:8765",
        authToken: "tok",
        librarianBase: "http://127.0.0.1:11435",
      }),
      initRuntimeConfig: () => undefined,
      pollerOptions: {
        baseDelayMs: 1,
        maxDelayMs: 1,
        connectingBudgetMs: 100,
        recoveryIntervalMs: 1,
        now: () => 0,
        setTimeoutFn,
        clearTimeoutFn: () => undefined,
      },
    });

    await flushMicrotasks();
    await flushMicrotasks();

    expect(startCouncilServer).not.toHaveBeenCalled();
    // Native path still schedules readiness polling (not a silent no-op).
    expect(handle.poller().phase()).toBe("connecting");
    expect(loadInitialState.mock.calls.length).toBeGreaterThanOrEqual(1);

    handle.stop();
  });

  it("when native does not own, startCouncilServer is invoked once", async () => {
    const timers: Array<() => void> = [];
    const setTimeoutFn = ((callback: () => void) => {
      timers.push(callback);
      return timers.length;
    }) as unknown as typeof setTimeout;

    const startCouncilServer = vi.fn(async () => undefined);
    const handle = startWarRoomBackendReady({
      loadInitialState: async () => false,
      isTauri: () => true,
      nativeOwnsCouncilStartup: async () => false,
      startCouncilServer,
      getConfigForStartup: async () => ({
        apiBase: "http://127.0.0.1:8765",
        authToken: "tok",
        librarianBase: "",
      }),
      initRuntimeConfig: () => undefined,
      pollerOptions: {
        baseDelayMs: 1,
        maxDelayMs: 1,
        connectingBudgetMs: 50,
        recoveryIntervalMs: 1,
        now: () => 0,
        setTimeoutFn,
        clearTimeoutFn: () => undefined,
      },
    });

    await flushMicrotasks();
    await flushMicrotasks();
    expect(startCouncilServer).toHaveBeenCalledTimes(1);
    handle.stop();
  });
});
