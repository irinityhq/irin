import { api } from "./api";

/** Strict Gate 4 projection returned by Council's authenticated BFF. */
export type WatchRegistryRow = {
  name: string;
  tier: string;
  cooldown_ms: number;
  enabled: boolean;
  hard_killed_at: number | null;
  last_fire_at: number | null;
  fires_last_hour: number;
};

export type WatchTemperature = {
  value: number;
  level: "cold" | "warm" | "hot";
  fires_last_hour: number;
  fires_last_24h: number;
};

export type WatchFire = {
  id: number;
  sentinel: string;
  fired_at: number;
};

/** Finite decision set projected by Gateway ui-snapshot. */
export const WATCH_EXECUTE_DECISIONS = [
  "completed",
  "refused",
  "pending",
  "expired",
  "dismissed",
  "bound",
] as const;
export type WatchExecuteDecision = (typeof WATCH_EXECUTE_DECISIONS)[number];

/** v1 sole earned-execution action. */
export const WATCH_EXECUTE_ACTIONS = ["quarantine_producer"] as const;
export type WatchExecuteAction = (typeof WATCH_EXECUTE_ACTIONS)[number];

/** Exact receipt field set — unknown keys (e.g. raw_token) are rejected. */
const EXECUTE_RECEIPT_KEYS = [
  "token_id",
  "decision",
  "action",
  "result",
  "directive_id",
  "in_response_to",
  "at_ms",
] as const;

/** Redacted Earned Execution receipt — no raw token/signature material. */
export type WatchExecuteReceipt = {
  token_id: string;
  decision: WatchExecuteDecision;
  action: WatchExecuteAction;
  /** Only `"acked"` or a ProblemDetails title; null for lifecycle-only decisions. */
  result: string | null;
  directive_id: string;
  in_response_to: string | null;
  at_ms: number;
};

export type WatchBudget = {
  spend_today_usd: number;
  spend_cap_usd: number;
};

export type WatchDegradation = {
  audit_infra_errors_total: number;
  persist_failures_total: number;
  pending_records: number;
  pending_retry_failures_total: number;
  pending_oldest_age_ms: number;
  lease_expired_during_deliberation_total: number;
  duplicate_charge_alarms_total: number;
  directive_ttl_expired_total: number;
  directive_max_delivery_exceeded_total: number;
  directive_clock_skew_rejected_total: number;
  recon_divergence_total: number;
  recon_cap_breach_total: number;
  settle_ceiling_overshoot_total: number;
  spend_gauge_read_failures_total: number;
  kill_switch_drain_timeout_total: number;
};

export type WatchSnapshot = {
  tenant: string;
  canary_tenant: string;
  action_production_armed: boolean;
  sentinels: WatchRegistryRow[];
  temperature: WatchTemperature;
  recent_fires: WatchFire[];
  recent_execute_receipts: WatchExecuteReceipt[];
  budget: WatchBudget;
  degradation: WatchDegradation;
};

export type CooldownState = "hard-killed" | "disabled" | "cooldown" | "ready";

/** Runtime pin for the BFF contract; never fall back to a hard-coded tenant. */
export function parseWatchSnapshot(value: unknown): WatchSnapshot {
  if (!value || typeof value !== "object") throw new Error("invalid Watch snapshot");
  const obj = value as Record<string, unknown>;
  if (typeof obj.canary_tenant !== "string" || !obj.canary_tenant) {
    throw new Error("Watch snapshot missing configured canary tenant");
  }
  if (obj.tenant !== obj.canary_tenant) {
    throw new Error("Watch snapshot tenant does not match configured canary");
  }
  if (typeof obj.action_production_armed !== "boolean") {
    throw new Error("Watch snapshot missing action-production state");
  }
  if (
    !Array.isArray(obj.sentinels) ||
    !Array.isArray(obj.recent_fires) ||
    !Array.isArray(obj.recent_execute_receipts)
  ) {
    throw new Error("Watch snapshot missing safe collection fields");
  }
  if (!obj.temperature || !obj.budget || !obj.degradation) {
    throw new Error("Watch snapshot missing readiness fields");
  }
  for (const receipt of obj.recent_execute_receipts) {
    assertRedactedExecuteReceipt(receipt);
  }
  return value as WatchSnapshot;
}

function isWatchExecuteDecision(value: unknown): value is WatchExecuteDecision {
  return (
    typeof value === "string" &&
    (WATCH_EXECUTE_DECISIONS as readonly string[]).includes(value)
  );
}

function isWatchExecuteAction(value: unknown): value is WatchExecuteAction {
  return (
    typeof value === "string" &&
    (WATCH_EXECUTE_ACTIONS as readonly string[]).includes(value)
  );
}

/**
 * Fail closed: exact seven-field whitelist, finite decision/action sets,
 * result only null | "acked" | non-empty ProblemDetails title.
 */
function assertRedactedExecuteReceipt(value: unknown): asserts value is WatchExecuteReceipt {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("invalid execute receipt");
  }
  const row = value as Record<string, unknown>;
  const keys = Object.keys(row);
  if (keys.length !== EXECUTE_RECEIPT_KEYS.length) {
    throw new Error("execute receipt field set invalid");
  }
  for (const key of EXECUTE_RECEIPT_KEYS) {
    if (!(key in row)) {
      throw new Error(`execute receipt missing ${key}`);
    }
  }
  for (const key of keys) {
    if (!(EXECUTE_RECEIPT_KEYS as readonly string[]).includes(key)) {
      throw new Error(`execute receipt unknown field: ${key}`);
    }
  }
  if (typeof row.token_id !== "string" || !row.token_id) {
    throw new Error("execute receipt token_id invalid");
  }
  if (!isWatchExecuteDecision(row.decision)) {
    throw new Error("execute receipt decision invalid");
  }
  if (!isWatchExecuteAction(row.action)) {
    throw new Error("execute receipt action invalid");
  }
  if (row.result != null) {
    if (typeof row.result !== "string" || !row.result.trim()) {
      throw new Error("execute receipt result invalid");
    }
    if (row.decision === "completed" && row.result !== "acked") {
      throw new Error("execute receipt completed result must be acked");
    }
    if (row.decision !== "completed" && row.decision !== "refused") {
      throw new Error("execute receipt result only allowed for completed/refused");
    }
  } else if (row.decision === "completed" || row.decision === "refused") {
    throw new Error("execute receipt result required for completed/refused");
  }
  if (typeof row.directive_id !== "string" || !row.directive_id) {
    throw new Error("execute receipt directive_id invalid");
  }
  if (row.in_response_to != null && typeof row.in_response_to !== "string") {
    throw new Error("execute receipt in_response_to invalid");
  }
  if (typeof row.at_ms !== "number" || !Number.isFinite(row.at_ms)) {
    throw new Error("execute receipt at_ms invalid");
  }
}

/** Derive operator-facing cooldown state from safe readiness fields. */
export function deriveCooldownState(
  row: Pick<WatchRegistryRow, "enabled" | "hard_killed_at" | "last_fire_at" | "cooldown_ms">,
  nowMs: number,
): CooldownState {
  if (row.hard_killed_at != null) return "hard-killed";
  if (!row.enabled) return "disabled";
  if (row.last_fire_at != null && nowMs - row.last_fire_at < row.cooldown_ms) {
    return "cooldown";
  }
  return "ready";
}

/** Read the one authenticated, server-projected Watch snapshot. */
export async function fetchWatchSnapshot(): Promise<WatchSnapshot> {
  return parseWatchSnapshot(await api.governanceWatch());
}
