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
  if (!Array.isArray(obj.sentinels) || !Array.isArray(obj.recent_fires)) {
    throw new Error("Watch snapshot missing safe collection fields");
  }
  if (!obj.temperature || !obj.budget || !obj.degradation) {
    throw new Error("Watch snapshot missing readiness fields");
  }
  return value as WatchSnapshot;
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
