import { describe, expect, it } from "vitest";
import {
  armedUntilLabel,
  assertNoSecretFields,
  deriveTouchIdView,
  touchIdArmSuccessMessage,
  touchIdReasonDetail,
  TOUCH_ID_FORBIDDEN_FIELDS,
  type TouchIdStatus,
} from "./touch-id";

function status(over: Partial<TouchIdStatus> = {}): TouchIdStatus {
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

describe("Touch ID state mapping", () => {
  it("offers Set up Touch ID when enrollment is absent", () => {
    const view = deriveTouchIdView(
      status({
        state: "setup_required",
        reason: "enrollment_missing",
        enrolled: false,
        can_enroll: true,
        can_arm: false,
      }),
    );
    expect(view.primaryLabel).toBe("Set up Touch ID");
    expect(view.primaryAction).toBe("enroll");
    expect(view.primaryEnabled).toBe(true);
  });

  it("explains that fresh enrollment waits for Gateway", () => {
    const view = deriveTouchIdView(
      status({
        state: "setup_required",
        reason: "gateway_not_ready",
        enrolled: false,
        can_enroll: false,
        can_arm: false,
      }),
    );
    expect(view.primaryEnabled).toBe(false);
    expect(view.detail).toContain("Enable Gateway");
  });

  it("offers Touch ID ready / Arm with Touch ID when enrolled and disarmed", () => {
    const view = deriveTouchIdView(status({ can_arm: true }));
    expect(view.label).toBe("Touch ID ready");
    expect(view.primaryLabel).toBe("Arm with Touch ID");
    expect(view.primaryEnabled).toBe(true);
  });

  it("renders a distinct panel state after rehearsal-ok", () => {
    const view = deriveTouchIdView(
      status({
        state: "ready",
        reason: "rehearsal_only_build",
        allow_real_arm: false,
        rehearsal_passed: true,
        can_arm: true,
      }),
    );
    expect(view.label).toBe("Rehearsal passed — not armed");
    expect(view.label).not.toBe("Touch ID ready");
    expect(view.primaryLabel).toBe("Arm with Touch ID");
    expect(view.primaryEnabled).toBe(true);
  });

  it("renders Armed until <time> with Renew and Disarm", () => {
    const exp = Date.now() + 600_000;
    const view = deriveTouchIdView(
      status({
        state: "armed",
        armed_exp_at_ms: exp,
        can_arm: false,
        can_renew: true,
        can_disarm: true,
      }),
    );
    expect(view.label).toMatch(/^Armed until /);
    expect(view.primaryLabel).toBe("Renew");
    expect(view.showDisarm).toBe(true);
  });

  it("never renders an armed lease as open-ended when the deadline is missing", () => {
    expect(armedUntilLabel(null)).toContain("unavailable");
    expect(armedUntilLabel(undefined)).toContain("unavailable");
  });

  it("offers Re-enroll Touch ID for every incompatibility, and never an arm", () => {
    for (const reason of [
      "registry_unloaded",
      "registry_mismatch",
      "helper_identity_changed",
      "enclave_key_missing",
    ] as const) {
      const view = deriveTouchIdView(
        status({
          state: "reenroll_required",
          reason,
          can_enroll: true,
          can_arm: false,
        }),
      );
      expect(view.primaryAction).toBe("enroll");
      expect(view.primaryLabel).toContain("Re-enroll");
      expect(view.primaryLabel).not.toContain("Arm");
    }
  });

  it("disables the arm action with a precise reason when prerequisites fail", () => {
    const view = deriveTouchIdView(
      status({
        state: "blocked",
        reason: "gateway_not_ready",
        can_arm: false,
      }),
    );
    expect(view.primaryEnabled).toBe(false);
    expect(view.detail).toBeTruthy();
    expect(touchIdReasonDetail("gateway_not_ready")).toContain("Enable Gateway");
  });

  it("keeps Disarm reachable while a ceremony is open", () => {
    const view = deriveTouchIdView(
      status({
        state: "ceremony_open",
        can_arm: true,
        can_disarm: true,
      }),
    );
    expect(view.showDisarm).toBe(true);
  });

  it("offers nothing when the helper is missing", () => {
    const view = deriveTouchIdView(
      status({
        state: "unavailable",
        reason: "helper_missing",
        can_arm: false,
        can_enroll: false,
      }),
    );
    expect(view.primaryAction).toBe("none");
    expect(view.primaryLabel).toBeNull();
    expect(view.showDisarm).toBe(false);
  });

  it("labels a rehearsal-only build without hiding the ceremony", () => {
    const view = deriveTouchIdView(
      status({ reason: "rehearsal_only_build", allow_real_arm: false }),
    );
    expect(view.primaryEnabled).toBe(true);
    expect(view.detail).toContain("rehearsal");
    // Pre-ceremony dirty build still shows ready, not "Rehearsal passed".
    expect(view.label).toBe("Touch ID ready");
  });

  it("maps rehearsal-only arm success toast without claiming Armed", () => {
    expect(
      touchIdArmSuccessMessage(
        status({
          state: "ready",
          reason: "rehearsal_only_build",
          allow_real_arm: false,
          rehearsal_passed: true,
        }),
      ),
    ).toBe("Rehearsal passed");
    expect(
      touchIdArmSuccessMessage(
        status({
          state: "ready",
          reason: "rehearsal_only_build",
          allow_real_arm: false,
        }),
      ),
    ).not.toContain("Armed");
  });

  it("does not claim a dirty-host rehearsal armed an inherited live lease", () => {
    const exp = Date.now() + 600_000;
    expect(
      touchIdArmSuccessMessage(
        status({
          state: "armed",
          reason: "rehearsal_only_build",
          allow_real_arm: false,
          armed_exp_at_ms: exp,
          can_renew: true,
          can_disarm: true,
          can_arm: false,
        }),
      ),
    ).toBe("Rehearsal passed");
  });

  it("maps clean-build real arm success toast as Armed", () => {
    const exp = Date.now() + 600_000;
    expect(
      touchIdArmSuccessMessage(
        status({
          state: "armed",
          allow_real_arm: true,
          armed_exp_at_ms: exp,
          can_renew: true,
          can_disarm: true,
          can_arm: false,
        }),
      ),
    ).toBe("Armed");
  });

  it("shows an expired lease as re-armable, not as armed", () => {
    const view = deriveTouchIdView(
      status({ reason: "lease_expired", can_arm: true, can_disarm: true }),
    );
    expect(view.state).toBe("ready");
    expect(view.primaryLabel).toBe("Arm with Touch ID");
    expect(view.detail).toContain("expired");
  });

  it("renders a safe placeholder before the first status arrives", () => {
    const view = deriveTouchIdView(null);
    expect(view.primaryAction).toBe("none");
    expect(view.primaryEnabled).toBe(false);
  });
});

describe("renderer redaction guard", () => {
  it("accepts the legitimate projection", () => {
    expect(() => assertNoSecretFields(status())).not.toThrow();
  });

  it("rejects every forbidden field, at any depth", () => {
    for (const field of TOUCH_ID_FORBIDDEN_FIELDS) {
      expect(() =>
        assertNoSecretFields({ ...status(), [field]: "x" }),
      ).toThrow(/must not carry/);
      expect(() =>
        assertNoSecretFields({ nested: { [field]: "x" } }),
      ).toThrow(/must not carry/);
      expect(() =>
        assertNoSecretFields({ list: [{ [field]: "x" }] }),
      ).toThrow(/must not carry/);
    }
  });

  it("is case-insensitive so a renamed native field cannot slip through", () => {
    expect(() => assertNoSecretFields({ Challenge: "x" })).toThrow();
  });

  it("fails the view derivation itself, not just the assertion", () => {
    expect(() =>
      deriveTouchIdView({
        ...status(),
        // @ts-expect-error intentional poison field
        challenge: "nope",
      }),
    ).toThrow();
  });
});
