import { describe, expect, it } from "vitest";
import {
  armedUntilLabel,
  assertNoSecretFields,
  beginTouchIdDisarm,
  createTouchIdOperationFence,
  deriveTouchIdView,
  endTouchIdDisarm,
  invalidateTouchIdStatusOperations,
  runTouchIdStatusWriterIfCurrent,
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
    ...over,
  };
}

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
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
    expect(view.showDisarm).toBe(false);
  });

  it("explains that fresh enrollment waits for Gateway", () => {
    const view = deriveTouchIdView(
      status({
        state: "setup_required",
        reason: "enrollment_missing",
        enrolled: false,
        can_enroll: false,
        can_arm: false,
      }),
    );
    expect(view.primaryEnabled).toBe(false);
    expect(view.detail).toBe("Enable Gateway first, then set up Touch ID.");
  });

  it("offers Touch ID ready / Arm with Touch ID when enrolled and disarmed", () => {
    const view = deriveTouchIdView(status());
    expect(view.label).toBe("Touch ID ready");
    expect(view.primaryLabel).toBe("Arm with Touch ID");
    expect(view.primaryAction).toBe("arm");
    expect(view.primaryEnabled).toBe(true);
  });

  it("renders Armed until <time> with Renew and Disarm", () => {
    const exp = new Date(2026, 6, 24, 14, 5, 0).getTime();
    const view = deriveTouchIdView(
      status({
        state: "armed",
        armed_exp_at_ms: exp,
        armed_expires_in_ms: 600_000,
        can_arm: false,
        can_renew: true,
        can_disarm: true,
      }),
    );
    expect(view.label).toBe("Armed until 14:05");
    expect(view.primaryLabel).toBe("Renew");
    expect(view.primaryAction).toBe("renew");
    expect(view.showDisarm).toBe(true);
    expect(view.armedUntilMs).toBe(exp);
  });

  it("never renders an armed lease as open-ended when the deadline is missing", () => {
    expect(armedUntilLabel(null)).toBe("Armed — deadline unavailable");
    expect(armedUntilLabel(0)).toBe("Armed — deadline unavailable");
    expect(armedUntilLabel(Number.NaN)).toBe("Armed — deadline unavailable");
  });

  it("offers Re-enroll Touch ID for every incompatibility, and never an arm", () => {
    for (const reason of [
      "helper_identity_changed",
      "enclave_key_missing",
      "registry_unloaded",
      "registry_mismatch",
      "enrollment_missing",
    ] as const) {
      const view = deriveTouchIdView(
        status({
          state: "reenroll_required",
          reason,
          can_enroll: true,
          can_arm: false,
        }),
      );
      expect(view.primaryLabel, reason).toBe("Re-enroll Touch ID");
      expect(view.primaryAction, reason).toBe("enroll");
      expect(view.detail, reason).toBeTruthy();
    }
  });

  it("disables the arm action with a precise reason when prerequisites fail", () => {
    for (const reason of [
      "gateway_not_ready",
      "watch_surface_unreachable",
      "arm_principal_missing",
    ] as const) {
      const view = deriveTouchIdView(
        status({ state: "blocked", reason, can_arm: false }),
      );
      expect(view.primaryLabel, reason).toBe("Arm with Touch ID");
      expect(view.primaryEnabled, reason).toBe(false);
      expect(view.detail, reason).toBe(touchIdReasonDetail(reason));
      expect(view.detail, reason).not.toBeNull();
    }
  });

  it("keeps Disarm reachable while a ceremony is open", () => {
    const view = deriveTouchIdView(
      status({
        state: "ceremony_open",
        stage_expires_in_ms: 90_000,
        can_disarm: true,
      }),
    );
    expect(view.label).toBe("Waiting for Touch ID…");
    expect(view.showDisarm).toBe(true);
  });

  it("offers nothing when the helper is missing", () => {
    const view = deriveTouchIdView(
      status({
        state: "unavailable",
        reason: "helper_missing",
        enrolled: false,
        can_arm: false,
      }),
    );
    expect(view.primaryAction).toBe("none");
    expect(view.primaryEnabled).toBe(false);
    expect(view.showDisarm).toBe(false);
  });

  it("labels a rehearsal-only build without hiding the ceremony", () => {
    const view = deriveTouchIdView(
      status({ reason: "rehearsal_only_build", allow_real_arm: false }),
    );
    expect(view.primaryEnabled).toBe(true);
    expect(view.detail).toContain("rehearsal");
  });

  it("maps rehearsal-only arm success toast without claiming Armed", () => {
    expect(
      touchIdArmSuccessMessage(
        status({
          state: "ready",
          reason: "rehearsal_only_build",
          allow_real_arm: false,
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

describe("Touch ID renderer operation ordering", () => {
  it("still applies a current initial or polling refresh", async () => {
    const fence = createTouchIdOperationFence();
    const current = status({ state: "ready", can_arm: true });
    let visibleStatus: TouchIdStatus | null = null;

    const outcome = await runTouchIdStatusWriterIfCurrent(
      fence,
      () => Promise.resolve(current),
      (next) => {
        visibleStatus = next;
      },
      () => {
        visibleStatus = null;
      },
    );

    expect(outcome).toBe("applied");
    expect(visibleStatus).toEqual(current);
  });

  it("does not let a blocked Arm completion overwrite Disarm or toast Armed", async () => {
    const fence = createTouchIdOperationFence();
    const arm = deferred<TouchIdStatus>();
    const armed = status({
      state: "armed",
      can_arm: false,
      can_renew: true,
      can_disarm: true,
      armed_exp_at_ms: Date.now() + 600_000,
    });
    const disarmed = status({
      state: "ready",
      can_arm: true,
      can_renew: false,
      can_disarm: false,
    });
    let visibleStatus = status({ state: "ceremony_open", can_disarm: true });
    const toasts: string[] = [];

    const armCompletion = runTouchIdStatusWriterIfCurrent(
      fence,
      () => arm.promise,
      (next) => {
        visibleStatus = next;
        toasts.push(touchIdArmSuccessMessage(next));
      },
      (error) => {
        toasts.push(error instanceof Error ? error.message : String(error));
      },
    );

    // The kill switch is clicked while the Arm promise is still blocked.
    invalidateTouchIdStatusOperations(fence);
    visibleStatus = disarmed;
    toasts.push("Disarmed");

    // Native Arm completes late. Its older generation must be invisible.
    arm.resolve(armed);
    expect(await armCompletion).toBe("stale");
    expect(visibleStatus).toEqual(disarmed);
    expect(toasts).toEqual(["Disarmed"]);
    expect(toasts).not.toContain("Armed");
  });

  it("does not let a refresh begun before Disarm repaint an older armed snapshot", async () => {
    const fence = createTouchIdOperationFence();
    const refresh = deferred<TouchIdStatus>();
    const armedSnapshot = status({
      state: "armed",
      can_arm: false,
      can_renew: true,
      can_disarm: true,
      armed_exp_at_ms: Date.now() + 600_000,
    });
    const disarmed = status({
      state: "ready",
      can_arm: true,
      can_renew: false,
      can_disarm: false,
    });
    let visibleStatus: TouchIdStatus | null = status({
      state: "armed",
      can_disarm: true,
    });

    const refreshCompletion = runTouchIdStatusWriterIfCurrent(
      fence,
      () => refresh.promise,
      (next) => {
        visibleStatus = next;
      },
      () => {
        visibleStatus = null;
      },
    );

    // The initial/polling read is still pending when Disarm takes ownership.
    invalidateTouchIdStatusOperations(fence);
    visibleStatus = disarmed;

    refresh.resolve(armedSnapshot);
    expect(await refreshCompletion).toBe("stale");
    expect(visibleStatus).toEqual(disarmed);
  });

  it("rejects a polling refresh that starts while native Disarm is pending", async () => {
    const fence = createTouchIdOperationFence();
    const disarmed = status({
      state: "ready",
      can_arm: true,
      can_renew: false,
      can_disarm: false,
    });
    let visibleStatus: TouchIdStatus | null = disarmed;
    let nativeReadStarted = false;

    beginTouchIdDisarm(fence);
    const outcome = await runTouchIdStatusWriterIfCurrent(
      fence,
      async () => {
        nativeReadStarted = true;
        return status({ state: "armed", can_disarm: true });
      },
      (next) => {
        visibleStatus = next;
      },
      () => {
        visibleStatus = null;
      },
    );

    expect(outcome).toBe("stale");
    expect(nativeReadStarted).toBe(false);
    expect(visibleStatus).toEqual(disarmed);

    endTouchIdDisarm(fence);
    expect(fence.disarmInProgress).toBe(false);
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
        field,
      ).toThrow(/must not carry/);
      expect(() =>
        assertNoSecretFields({ ...status(), nested: { [field]: "x" } }),
        field,
      ).toThrow(/must not carry/);
      expect(() =>
        assertNoSecretFields({ ...status(), list: [{ [field]: "x" }] }),
        field,
      ).toThrow(/must not carry/);
    }
  });

  it("is case-insensitive so a renamed native field cannot slip through", () => {
    expect(() => assertNoSecretFields({ ...status(), Challenge: "x" })).toThrow();
    expect(() => assertNoSecretFields({ ...status(), KEYSET_HASH: "x" })).toThrow();
  });

  it("fails the view derivation itself, not just the assertion", () => {
    expect(() =>
      deriveTouchIdView({ ...status(), challenge: "abc" } as TouchIdStatus),
    ).toThrow(/must not carry/);
  });
});
