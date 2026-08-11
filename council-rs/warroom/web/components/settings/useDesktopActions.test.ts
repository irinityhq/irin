import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  DesktopStatusSnapshot,
  GatewayPackStatus,
  PhoneAccessStatus,
  TouchIdStatus,
} from "@/lib/tauri";
import { runGatewayPackActionOnce } from "./useDesktopActions";

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

describe("runGatewayPackActionOnce (success updates then emits once; error zero)", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("on success: applySnapshot then exactly one emit, then onSuccess", async () => {
    const order: string[] = [];
    const next = snap({ seq: 7, pack: pack({ state: "authenticated_ready" }) });
    const applySnapshot = vi.fn((s: DesktopStatusSnapshot) => {
      order.push("apply");
      expect(s.seq).toBe(7);
    });
    const emit = vi.fn(() => {
      order.push("emit");
    });
    const onSuccess = vi.fn((status: GatewayPackStatus) => {
      order.push("success");
      expect(status.state).toBe("authenticated_ready");
    });
    const onError = vi.fn();

    const result = await runGatewayPackActionOnce(
      async () => next,
      applySnapshot,
      onSuccess,
      onError,
      emit,
    );

    expect(result).toBe("ok");
    expect(order).toEqual(["apply", "emit", "success"]);
    expect(applySnapshot).toHaveBeenCalledTimes(1);
    expect(emit).toHaveBeenCalledTimes(1);
    expect(onSuccess).toHaveBeenCalledTimes(1);
    expect(onError).not.toHaveBeenCalled();
  });

  it("on error: emits zero times and never applies snapshot", async () => {
    const applySnapshot = vi.fn();
    const emit = vi.fn();
    const onSuccess = vi.fn();
    const onError = vi.fn();

    const result = await runGatewayPackActionOnce(
      async () => {
        throw new Error("Touch ID cancelled");
      },
      applySnapshot,
      onSuccess,
      onError,
      emit,
    );

    expect(result).toBe("error");
    expect(applySnapshot).not.toHaveBeenCalled();
    expect(emit).not.toHaveBeenCalled();
    expect(onSuccess).not.toHaveBeenCalled();
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith("Touch ID cancelled");
  });
});
