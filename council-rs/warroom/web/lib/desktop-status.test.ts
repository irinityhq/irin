import { describe, expect, it } from "vitest";
import { applyIfNewer, mergeIfNewer, type DesktopStatusSnapshot } from "./desktop-status";
import type { GatewayPackStatus, PhoneAccessStatus, TouchIdStatus } from "./tauri";

function pack(over: Partial<GatewayPackStatus> = {}): GatewayPackStatus {
  return {
    state: "disabled",
    message: "test",
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
    council_governed: false,
    gateway_url_configured: true,
    support_matrix_summary: "",
    spawn_capable: false,
    governed_ready: false,
    hard_down: true,
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

describe("applyIfNewer", () => {
  it("accepts the first snapshot when prev is null", () => {
    const next = snap({ seq: 1 });
    expect(applyIfNewer(null, next)).toEqual(next);
  });

  it("no-ops when next.seq is older", () => {
    const prev = snap({ seq: 5, touch_id: touch({ state: "armed" }) });
    const older = snap({ seq: 3, touch_id: touch({ state: "ready" }) });
    expect(applyIfNewer(prev, older)).toBeNull();
  });

  it("no-ops when next.seq is equal", () => {
    const prev = snap({ seq: 5 });
    const same = snap({ seq: 5, pack: pack({ message: "stale" }) });
    expect(applyIfNewer(prev, same)).toBeNull();
  });

  it("accepts a newer seq in the same epoch", () => {
    const prev = snap({ seq: 5, touch_id: touch({ state: "ready" }) });
    const next = snap({ seq: 6, touch_id: touch({ state: "armed" }) });
    expect(applyIfNewer(prev, next)).toEqual(next);
  });

  it("resets on authority_epoch change even with lower seq", () => {
    const prev = snap({ authority_epoch: "epoch-a", seq: 99 });
    const next = snap({ authority_epoch: "epoch-b", seq: 1 });
    expect(applyIfNewer(prev, next)).toEqual(next);
  });

  it("action snapshot beats a late older event", () => {
    // Action returned seq=10; a late background event with seq=9 must not paint.
    const afterAction = snap({
      seq: 10,
      touch_id: touch({ state: "armed", can_disarm: true }),
    });
    const lateBackground = snap({
      seq: 9,
      touch_id: touch({ state: "ready", can_arm: true }),
    });
    expect(applyIfNewer(afterAction, lateBackground)).toBeNull();
    // Event arriving after action with higher seq is accepted.
    const later = snap({
      seq: 11,
      touch_id: touch({ state: "armed", can_disarm: true }),
    });
    expect(applyIfNewer(afterAction, later)).toEqual(later);
  });

  it("event and command result converge in either arrival order", () => {
    const command = snap({
      seq: 4,
      touch_id: touch({ state: "armed" }),
    });
    const event = snap({
      seq: 4,
      touch_id: touch({ state: "armed" }),
    });
    // Same seq: second arrival is a no-op either way.
    expect(applyIfNewer(command, event)).toBeNull();
    expect(applyIfNewer(event, command)).toBeNull();

    const olderEvent = snap({ seq: 3, touch_id: touch({ state: "ready" }) });
    const newerCommand = snap({ seq: 5, touch_id: touch({ state: "armed" }) });
    // Event first, then command.
    let painted = applyIfNewer(null, olderEvent)!;
    painted = applyIfNewer(painted, newerCommand)!;
    expect(painted.seq).toBe(5);
    expect(painted.touch_id.state).toBe("armed");
    // Command first, then older event.
    painted = applyIfNewer(null, newerCommand)!;
    expect(applyIfNewer(painted, olderEvent)).toBeNull();
    expect(painted.touch_id.state).toBe("armed");
  });
});

describe("mergeIfNewer", () => {
  it("keeps prev when next is stale", () => {
    const prev = snap({ seq: 3 });
    const older = snap({ seq: 1, pack: pack({ message: "old" }) });
    expect(mergeIfNewer(prev, older)).toEqual(prev);
  });

  it("replaces when next is newer", () => {
    const prev = snap({ seq: 1 });
    const next = snap({ seq: 2 });
    expect(mergeIfNewer(prev, next)).toEqual(next);
  });
});
