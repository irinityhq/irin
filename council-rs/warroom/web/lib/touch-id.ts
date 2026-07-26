/**
 * Touch ID product control — presentation mapping.
 *
 * Pure functions over the native `TouchIdStatus` projection. This module never
 * talks to the native host and never holds ceremony material: the renderer's
 * whole job is to turn a state + reason into a label, an action, and (when a
 * lease is live) a wall-clock deadline.
 */

import type { TouchIdReason, TouchIdState, TouchIdStatus } from "./tauri";

export type { TouchIdReason, TouchIdState, TouchIdStatus };

/**
 * Renderer-side ordering fence for asynchronous Touch ID status writers.
 *
 * The native host owns the security boundary. This small renderer fence owns
 * presentation ordering: once Disarm is clicked, no older Arm/Renew completion
 * or status refresh can repaint the UI or emit a stale toast.
 */
export interface TouchIdOperationFence {
  generation: number;
  disarmInProgress: boolean;
}

export function createTouchIdOperationFence(): TouchIdOperationFence {
  return { generation: 0, disarmInProgress: false };
}

/** Invalidate every status result already awaiting native work. */
export function invalidateTouchIdStatusOperations(
  fence: TouchIdOperationFence,
): void {
  fence.generation += 1;
}

/** Block all status writers for the entire native Disarm boundary. */
export function beginTouchIdDisarm(fence: TouchIdOperationFence): void {
  fence.disarmInProgress = true;
  invalidateTouchIdStatusOperations(fence);
}

/** Reopen status writers only after invalidating reads begun during Disarm. */
export function endTouchIdDisarm(fence: TouchIdOperationFence): void {
  invalidateTouchIdStatusOperations(fence);
  fence.disarmInProgress = false;
}

/**
 * Apply an asynchronous status result only when no newer Disarm invalidated it.
 *
 * Errors from an invalidated operation are ignored too: the newer Disarm
 * result and its toast are the renderer's authoritative visible outcome.
 */
export async function runTouchIdStatusWriterIfCurrent(
  fence: TouchIdOperationFence,
  action: () => Promise<TouchIdStatus>,
  onSuccess: (status: TouchIdStatus) => void,
  onError: (error: unknown) => void,
): Promise<"applied" | "stale"> {
  if (fence.disarmInProgress) return "stale";
  const generation = fence.generation;
  try {
    const status = await action();
    if (fence.disarmInProgress || generation !== fence.generation) return "stale";
    onSuccess(status);
  } catch (error) {
    if (fence.disarmInProgress || generation !== fence.generation) return "stale";
    onError(error);
  }
  return "applied";
}

/**
 * Field names that must NEVER appear on a status object reaching the renderer.
 * `assertNoSecretFields` is called on every status the control renders, so a
 * future native change that widens the projection fails loudly in tests and in
 * development rather than silently leaking into the DOM.
 */
export const TOUCH_ID_FORBIDDEN_FIELDS = [
  "challenge",
  "signature",
  "signature_der",
  "attestation",
  "authenticator_data",
  "client_data_json",
  "credential_id",
  "public_key",
  "private_key",
  "key_blob",
  "keyset_hash",
  "principal",
  "principals",
  "token",
  "admin_token",
  "bearer",
  "registry",
] as const;

/**
 * Fail-closed redaction guard. Returns the status unchanged when it is clean;
 * throws when the native side has handed the renderer something it must not
 * have. Deep-checks nested objects so a wrapped payload cannot slip through.
 */
export function assertNoSecretFields(status: unknown, path = "status"): void {
  if (status === null || typeof status !== "object") return;
  if (Array.isArray(status)) {
    status.forEach((v, i) => assertNoSecretFields(v, `${path}[${i}]`));
    return;
  }
  for (const [key, value] of Object.entries(status as Record<string, unknown>)) {
    const lowered = key.toLowerCase();
    if ((TOUCH_ID_FORBIDDEN_FIELDS as readonly string[]).includes(lowered)) {
      throw new Error(`Touch ID status must not carry ${path}.${key}`);
    }
    assertNoSecretFields(value, `${path}.${key}`);
  }
}

/** The action a click on the primary button performs. */
export type TouchIdAction = "enroll" | "arm" | "renew" | "none";

export interface TouchIdView {
  state: TouchIdState;
  /** Headline shown next to the Gateway control. */
  label: string;
  /** Primary button copy, or null when there is no primary action. */
  primaryLabel: string | null;
  primaryAction: TouchIdAction;
  primaryEnabled: boolean;
  /** Disarm is rendered as a separate, always-explicit control. */
  showDisarm: boolean;
  /** Precise operator-facing explanation when something is off. */
  detail: string | null;
  /** Present only while a lease is live. */
  armedUntilMs: number | null;
}

/** Human copy for a machine reason tag. */
export function touchIdReasonDetail(reason: TouchIdReason | null): string | null {
  switch (reason) {
    case "helper_missing":
      return "The Touch ID signing helper is not bundled in this app build.";
    case "gateway_not_ready":
      return "Enable Gateway first — the Watch surface lives in the Gateway Pack.";
    case "watch_surface_unreachable":
      return "Gateway is up but the Watch surface did not answer. Try again once the pack finishes starting.";
    case "arm_principal_missing":
      return "This app has no arm principal on the running pack. Restart the Gateway Pack after enrolling.";
    case "registry_unloaded":
      return "The running Gateway Pack has no usable enrollment. Re-enroll, then restart the pack.";
    case "registry_mismatch":
      return "The running Gateway Pack loaded a different enrollment than this app recorded.";
    case "helper_identity_changed":
      return "The signing helper changed since enrollment. Secure Enclave continuity cannot be proven.";
    case "enclave_key_missing":
      return "The Secure Enclave key for this enrollment is gone. A wrapped key is never reused across enclaves.";
    case "enrollment_missing":
      return "No enrollment recorded by this app.";
    case "rehearsal_only_build":
      return "This build may only run a rehearsal ceremony; the producer will not start.";
    case "lease_expired":
      return "The previous arm window expired. Arming again requires a new Touch ID tap.";
    default:
      return null;
  }
}

/**
 * The single source of the control's presentation. Derived only from the
 * native projection — the renderer never infers state from a timer or from the
 * Gateway control's own state.
 */
export function deriveTouchIdView(
  status: TouchIdStatus | null | undefined,
): TouchIdView {
  if (!status) {
    return {
      state: "unavailable",
      label: "Touch ID",
      primaryLabel: null,
      primaryAction: "none",
      primaryEnabled: false,
      showDisarm: false,
      detail: "Checking Touch ID availability…",
      armedUntilMs: null,
    };
  }
  assertNoSecretFields(status);

  const detail = touchIdReasonDetail(status.reason);
  const enrollmentDetail = status.can_enroll
    ? detail
    : "Enable Gateway first, then set up Touch ID.";

  switch (status.state) {
    case "unavailable":
      return {
        state: status.state,
        label: "Touch ID unavailable",
        primaryLabel: null,
        primaryAction: "none",
        primaryEnabled: false,
        showDisarm: false,
        detail,
        armedUntilMs: null,
      };
    case "setup_required":
      return {
        state: status.state,
        label: "Touch ID not set up",
        primaryLabel: "Set up Touch ID",
        primaryAction: "enroll",
        primaryEnabled: status.can_enroll,
        showDisarm: false,
        detail: enrollmentDetail,
        armedUntilMs: null,
      };
    case "reenroll_required":
      return {
        state: status.state,
        label: "Touch ID needs re-enrollment",
        primaryLabel: "Re-enroll Touch ID",
        primaryAction: "enroll",
        primaryEnabled: status.can_enroll,
        showDisarm: false,
        detail: enrollmentDetail,
        armedUntilMs: null,
      };
    case "blocked":
      return {
        state: status.state,
        label: "Touch ID ready",
        primaryLabel: "Arm with Touch ID",
        primaryAction: "arm",
        // Deliberately disabled rather than hidden: the operator should see the
        // control and the precise reason it cannot act.
        primaryEnabled: false,
        showDisarm: false,
        detail,
        armedUntilMs: null,
      };
    case "armed":
      return {
        state: status.state,
        label: armedUntilLabel(status.armed_exp_at_ms),
        primaryLabel: "Renew",
        primaryAction: "renew",
        primaryEnabled: status.can_renew,
        showDisarm: status.can_disarm,
        detail,
        armedUntilMs: status.armed_exp_at_ms,
      };
    case "ceremony_open":
      return {
        state: status.state,
        label: "Waiting for Touch ID…",
        primaryLabel: "Arm with Touch ID",
        primaryAction: "arm",
        primaryEnabled: status.can_arm,
        showDisarm: status.can_disarm,
        detail,
        armedUntilMs: null,
      };
    case "ready":
    default:
      return {
        state: "ready",
        label: "Touch ID ready",
        primaryLabel: "Arm with Touch ID",
        primaryAction: "arm",
        primaryEnabled: status.can_arm,
        showDisarm: status.can_disarm,
        detail,
        armedUntilMs: null,
      };
  }
}

/**
 * "Armed until 14:05". A missing deadline never renders as an open-ended arm:
 * an armed lease with no readable deadline says so.
 */
export function armedUntilLabel(expAtMs: number | null | undefined): string {
  if (typeof expAtMs !== "number" || !Number.isFinite(expAtMs) || expAtMs <= 0) {
    return "Armed — deadline unavailable";
  }
  const when = new Date(expAtMs);
  const hh = String(when.getHours()).padStart(2, "0");
  const mm = String(when.getMinutes()).padStart(2, "0");
  return `Armed until ${hh}:${mm}`;
}

/**
 * Success toast after a Touch ID arm ceremony completes without throwing.
 *
 * Real arm (clean build) lands in `armed` and says "Armed". A rehearsal-only
 * ready state must never claim "Armed" — the producer did not start.
 */
export function touchIdArmSuccessMessage(status: TouchIdStatus): string {
  // A dirty host can still observe a real lease created by a clean build. Its
  // local ceremony remains rehearsal-only, so build eligibility wins over the
  // inherited lease state when we describe the action that just completed.
  if (
    status.reason === "rehearsal_only_build" ||
    status.allow_real_arm === false
  ) {
    return "Rehearsal passed";
  }
  if (status.state === "armed") {
    return "Armed";
  }
  // Non-armed success without a rehearsal marker is still not a real arm.
  return "Rehearsal passed";
}
