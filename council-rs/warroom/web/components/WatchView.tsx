"use client";

import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  Flame,
  FolderOpen,
  Gauge,
  ShieldCheck,
  Thermometer,
  Wallet,
} from "lucide-react";
import { cn, fmtCost } from "@/lib/cn";
import {
  getWatchSentinelsEnabled,
  isTauri,
  openWatchInbox,
  setWatchSentinelsEnabled,
} from "@/lib/tauri";
import {
  deriveCooldownState,
  fetchWatchSnapshot,
  type CooldownState,
  type WatchDegradation,
  type WatchExecuteReceipt,
  type WatchFire,
  type WatchRegistryRow,
  type WatchSnapshot,
} from "@/lib/watch-gateway";

const POLL_MS = 10_000;

const COOLDOWN_TONE: Record<CooldownState, string> = {
  "hard-killed": "chip-danger",
  disabled: "chip-muted",
  cooldown: "chip-amber",
  ready: "chip-success",
};

const TEMP_TONE: Record<string, string> = {
  cold: "text-cyan",
  warm: "text-amber",
  hot: "text-danger",
};

export default function WatchView(
  { initialTenant: _initialTenant }: { initialTenant?: string } = {},
) {
  const [snapshot, setSnapshot] = useState<WatchSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [profileOn, setProfileOn] = useState(false);
  const [toggleBusy, setToggleBusy] = useState(false);
  const [toggleError, setToggleError] = useState<string | null>(null);
  const desktop = isTauri();

  const load = useCallback(async () => {
    try {
      const data = await fetchWatchSnapshot();
      setSnapshot(data);
      setNowMs(Date.now());
      setError(null);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  const refreshProfileFlag = useCallback(async () => {
    if (!desktop) return;
    try {
      setProfileOn(await getWatchSentinelsEnabled());
    } catch {
      // Non-fatal: snapshot still drives visibility.
    }
  }, [desktop]);

  useEffect(() => {
    void load();
    void refreshProfileFlag();
    const onConfig = () => {
      void load();
      void refreshProfileFlag();
    };
    window.addEventListener("warroom-config-changed", onConfig);
    const id = setInterval(() => {
      void load();
      void refreshProfileFlag();
    }, POLL_MS);
    return () => {
      clearInterval(id);
      window.removeEventListener("warroom-config-changed", onConfig);
    };
  }, [load, refreshProfileFlag]);

  const onToggle = async (next: boolean) => {
    if (!desktop || toggleBusy) return;
    setToggleBusy(true);
    setToggleError(null);
    try {
      const enabled = await setWatchSentinelsEnabled(next);
      setProfileOn(enabled);
      // Pack recreate drops the connection briefly; poll once after a beat.
      await new Promise((r) => setTimeout(r, 1500));
      await load();
    } catch (err: unknown) {
      setToggleError(err instanceof Error ? err.message : String(err));
      await refreshProfileFlag();
    } finally {
      setToggleBusy(false);
    }
  };

  const onOpenInbox = async () => {
    if (!desktop || toggleBusy) return;
    setToggleError(null);
    try {
      await openWatchInbox();
    } catch (err: unknown) {
      setToggleError(err instanceof Error ? err.message : String(err));
    }
  };

  const quietNoProfile =
    snapshot != null &&
    snapshot.sentinels.length === 0 &&
    !profileOn;

  return (
    <div data-testid="watch-view" className="space-y-5">
      <div className="flex items-end justify-between border-b border-border pb-3">
        <div>
          <h2 className="text-[11px] font-mono font-semibold uppercase tracking-widest text-fg-muted">
            Watch Plane
          </h2>
          <p className="text-sm text-fg-muted mt-1">
            Sentinel readiness, recent fires, redacted execute receipts, budget, and degradation
            counters. Profile toggle recreates the Gateway pack briefly.
          </p>
        </div>
        {snapshot && (
          <div className="text-right">
            <div className="label">Configured canary</div>
            <div className="text-xs font-mono text-fg-bright">{snapshot.canary_tenant}</div>
          </div>
        )}
      </div>

      {desktop && (
        <WatchEnablementBar
          profileOn={profileOn}
          busy={toggleBusy}
          error={toggleError}
          onToggle={onToggle}
          onOpenInbox={onOpenInbox}
        />
      )}

      {loading && !snapshot ? (
        <div className="panel p-12 text-center text-xs font-mono text-fg-dim animate-pulse">
          Fetching authenticated Watch snapshot…
        </div>
      ) : error && !snapshot ? (
        <GatewayPanel detail={error} />
      ) : snapshot ? (
        <>
          {error && (
            <div className="panel p-3 text-xs font-mono text-amber">
              Refresh failed; showing the last successful snapshot. {error}
            </div>
          )}
          {quietNoProfile && (
            <div
              data-testid="watch-quiet-reason"
              className="panel p-4 text-sm text-fg-muted"
            >
              <span className="font-mono text-[11px] uppercase tracking-widest text-fg-dim">
                Quiet
              </span>
              <p className="mt-1">
                No sentinel profile installed. Watch is healthy with 0 sentinels — flip{" "}
                <strong className="text-fg">Watch sentinels</strong> on to load the bundled
                file-inbox profile (tenant canary).
              </p>
            </div>
          )}
          <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
            <TemperatureCard snapshot={snapshot} />
            <BudgetCard snapshot={snapshot} />
            <ActivityCard snapshot={snapshot} />
          </div>
          <RegistryTable
            rows={snapshot.sentinels}
            nowMs={nowMs}
            quietNoProfile={quietNoProfile}
          />
          <FireLogTable fires={snapshot.recent_fires} />
          <ExecuteReceiptTable receipts={snapshot.recent_execute_receipts} />
          <DegradationPanel degradation={snapshot.degradation} />
        </>
      ) : null}
    </div>
  );
}

function WatchEnablementBar({
  profileOn,
  busy,
  error,
  onToggle,
  onOpenInbox,
}: {
  profileOn: boolean;
  busy: boolean;
  error: string | null;
  onToggle: (next: boolean) => void;
  onOpenInbox: () => void;
}) {
  return (
    <div
      data-testid="watch-enablement"
      className="panel p-4 flex flex-col md:flex-row md:items-center md:justify-between gap-3"
    >
      <div className="space-y-1">
        <div className="label">Watch sentinels</div>
        <p className="text-xs text-fg-muted">
          {profileOn
            ? "Profile installed under app-support. Toggling off removes it and recreates the pack."
            : "Off — no profile file. 0 sentinels is the normal quiet state."}
        </p>
        {error && (
          <p className="text-xs font-mono text-danger" data-testid="watch-toggle-error">
            {error}
          </p>
        )}
      </div>
      <div className="flex items-center gap-2 shrink-0">
        <button
          type="button"
          data-testid="watch-open-inbox"
          className="btn btn-ghost text-xs font-mono"
          disabled={busy}
          onClick={() => onOpenInbox()}
        >
          <FolderOpen className="w-3.5 h-3.5 mr-1 inline" />
          Open inbox folder
        </button>
        <button
          type="button"
          data-testid="watch-sentinels-toggle"
          className={cn("btn text-xs font-mono", profileOn ? "btn-primary" : "btn-ghost")}
          disabled={busy}
          aria-pressed={profileOn}
          onClick={() => onToggle(!profileOn)}
        >
          {busy ? "Working…" : profileOn ? "On" : "Off"}
        </button>
      </div>
    </div>
  );
}

function GatewayPanel({ detail }: { detail: string }) {
  return (
    <div className="panel p-6 space-y-2">
      <div className="flex items-center gap-2 text-[11px] font-mono font-semibold uppercase tracking-widest text-fg-muted">
        <ShieldCheck className="w-4 h-4 text-amber" />
        Governance snapshot unavailable
      </div>
      <p className="text-sm text-fg">
        Council could not read the authenticated Gateway projection.
      </p>
      <p className="text-xs font-mono text-fg-dim">{detail}</p>
    </div>
  );
}

function TemperatureCard({ snapshot }: { snapshot: WatchSnapshot }) {
  const temperature = snapshot.temperature;
  return (
    <div className="panel p-4 space-y-2">
      <div className="label flex items-center gap-1">
        <Thermometer className="w-3 h-3" /> Temperature
      </div>
      <div className={cn("text-2xl font-mono font-semibold", TEMP_TONE[temperature.level] ?? "text-fg")}>
        {(temperature.value * 100).toFixed(0)}%
        <span className="ml-2 text-xs uppercase tracking-widest">{temperature.level}</span>
      </div>
      <div className="text-[10px] font-mono text-fg-dim">
        {temperature.fires_last_hour} fires / 1h · {temperature.fires_last_24h} / 24h
      </div>
    </div>
  );
}

function BudgetCard({ snapshot }: { snapshot: WatchSnapshot }) {
  const budget = snapshot.budget;
  const pct = budget.spend_cap_usd > 0
    ? Math.min(100, (budget.spend_today_usd / budget.spend_cap_usd) * 100)
    : null;
  return (
    <div className="panel p-4 space-y-2">
      <div className="label flex items-center gap-1">
        <Wallet className="w-3 h-3" /> Budget burn (UTC day)
      </div>
      <div className="text-lg font-mono font-semibold text-fg-bright">
        {fmtCost(budget.spend_today_usd)}
        <span className="text-fg-dim font-normal"> / {fmtCost(budget.spend_cap_usd)} cap</span>
      </div>
      {pct != null && (
        <div className="h-1.5 rounded bg-bg-deep overflow-hidden">
          <div
            className={cn("h-full rounded", pct >= 90 ? "bg-danger" : pct >= 70 ? "bg-amber" : "bg-success")}
            style={{ width: `${pct}%` }}
          />
        </div>
      )}
    </div>
  );
}

function ActivityCard({ snapshot }: { snapshot: WatchSnapshot }) {
  const degraded = Object.values(snapshot.degradation).filter((value) => value > 0).length;
  return (
    <div className="panel p-4 space-y-2">
      <div className="label flex items-center gap-1"><Gauge className="w-3 h-3" /> Snapshot</div>
      <div className="text-[10px] font-mono text-fg-dim space-y-1">
        <div className={snapshot.action_production_armed ? "text-amber" : "text-success"}>
          Action production {snapshot.action_production_armed ? "ARMED" : "DISARMED"}
        </div>
        <div>{snapshot.sentinels.length} registered sentinel{snapshot.sentinels.length === 1 ? "" : "s"}</div>
        <div>{snapshot.recent_fires.length} recent fire identifier{snapshot.recent_fires.length === 1 ? "" : "s"}</div>
        <div>
          {snapshot.recent_execute_receipts.length} redacted execute receipt
          {snapshot.recent_execute_receipts.length === 1 ? "" : "s"}
        </div>
        <div className={degraded > 0 ? "text-amber" : "text-success"}>
          {degraded > 0 ? `${degraded} non-zero degradation counters` : "No degradation counters raised"}
        </div>
        <div>Poll every {POLL_MS / 1000}s</div>
      </div>
    </div>
  );
}

function RegistryTable({
  rows,
  nowMs,
  quietNoProfile,
}: {
  rows: WatchRegistryRow[];
  nowMs: number;
  quietNoProfile?: boolean;
}) {
  return (
    <section className="space-y-2">
      <header className="flex items-center gap-2 text-[10px] font-mono uppercase tracking-widest text-fg-dim">
        <Activity className="w-3.5 h-3.5" /> Registered watches
      </header>
      {rows.length === 0 ? (
        <div className="panel p-8 text-center text-sm text-fg-muted">
          {quietNoProfile
            ? "No sentinel profile installed."
            : "No sentinels registered for this canary."}
        </div>
      ) : (
        <div className="border border-border rounded overflow-hidden bg-bg-elevated">
          <table className="w-full text-left text-xs font-mono">
            <thead className="bg-bg-deep text-fg-dim uppercase tracking-wider text-[10px]">
              <tr>
                <th className="px-3 py-2 font-semibold">Name</th>
                <th className="px-3 py-2 font-semibold">Tier</th>
                <th className="px-3 py-2 font-semibold">State</th>
                <th className="px-3 py-2 font-semibold">Cooldown</th>
                <th className="px-3 py-2 font-semibold">Last fire</th>
                <th className="px-3 py-2 font-semibold text-right">Fires (1h)</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {rows.map((row) => {
                const state = deriveCooldownState(row, nowMs);
                return (
                  <tr key={row.name} className="hover:bg-bg-deep/50">
                    <td className="px-3 py-2 text-fg-bright font-semibold">{row.name}</td>
                    <td className="px-3 py-2"><span className="chip">{row.tier}</span></td>
                    <td className="px-3 py-2"><span className={cn("chip", COOLDOWN_TONE[state])}>{state}</span></td>
                    <td className="px-3 py-2 text-fg-dim">{row.cooldown_ms}ms</td>
                    <td className="px-3 py-2 text-fg-muted whitespace-nowrap">
                      {row.last_fire_at ? new Date(row.last_fire_at).toLocaleString() : "—"}
                    </td>
                    <td className="px-3 py-2 text-right text-fg">{row.fires_last_hour}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function FireLogTable({ fires }: { fires: WatchFire[] }) {
  return (
    <section className="space-y-2">
      <header className="flex items-center gap-2 text-[10px] font-mono uppercase tracking-widest text-fg-dim">
        <Flame className="w-3.5 h-3.5" /> Recent fire identifiers
      </header>
      {fires.length === 0 ? (
        <div className="panel p-8 text-center text-sm text-fg-muted">No fires recorded for this canary.</div>
      ) : (
        <div className="border border-border rounded overflow-hidden bg-bg-elevated">
          <table className="w-full text-left text-xs font-mono">
            <thead className="bg-bg-deep text-fg-dim uppercase tracking-wider text-[10px]">
              <tr><th className="px-3 py-2">ID</th><th className="px-3 py-2">Sentinel</th><th className="px-3 py-2">Fired</th></tr>
            </thead>
            <tbody className="divide-y divide-border">
              {fires.map((fire) => (
                <tr key={fire.id}>
                  <td className="px-3 py-2 text-fg-dim">{fire.id}</td>
                  <td className="px-3 py-2 text-fg-bright">{fire.sentinel}</td>
                  <td className="px-3 py-2 text-fg-muted">{new Date(fire.fired_at).toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function ExecuteReceiptTable({ receipts }: { receipts: WatchExecuteReceipt[] }) {
  return (
    <section className="space-y-2" data-testid="execute-receipts">
      <header className="flex items-center gap-2 text-[10px] font-mono uppercase tracking-widest text-fg-dim">
        <ShieldCheck className="w-3.5 h-3.5" /> Redacted execute receipts
      </header>
      {receipts.length === 0 ? (
        <div className="panel p-8 text-center text-sm text-fg-muted">
          No earned-execution receipts for this canary.
        </div>
      ) : (
        <div className="border border-border rounded overflow-hidden bg-bg-elevated">
          <table className="w-full text-left text-xs font-mono">
            <thead className="bg-bg-deep text-fg-dim uppercase tracking-wider text-[10px]">
              <tr>
                <th className="px-3 py-2">Token id</th>
                <th className="px-3 py-2">Decision</th>
                <th className="px-3 py-2">Action</th>
                <th className="px-3 py-2">Result</th>
                <th className="px-3 py-2">Directive</th>
                <th className="px-3 py-2">Correlation</th>
                <th className="px-3 py-2">At</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {receipts.map((receipt) => (
                <tr key={`${receipt.token_id}:${receipt.directive_id}`}>
                  <td className="px-3 py-2 text-fg-bright">{receipt.token_id}</td>
                  <td className="px-3 py-2">
                    <span
                      className={cn(
                        "chip",
                        receipt.decision === "completed"
                          ? "chip-success"
                          : receipt.decision === "refused"
                            ? "chip-danger"
                            : "chip-muted",
                      )}
                    >
                      {receipt.decision}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-fg">{receipt.action}</td>
                  <td className="px-3 py-2 text-fg-muted">{receipt.result ?? "—"}</td>
                  <td className="px-3 py-2 text-fg-dim">{receipt.directive_id}</td>
                  <td className="px-3 py-2 text-fg-dim">{receipt.in_response_to ?? "—"}</td>
                  <td className="px-3 py-2 text-fg-muted whitespace-nowrap">
                    {new Date(receipt.at_ms).toLocaleString()}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

const DEGRADATION_LABELS: Record<keyof WatchDegradation, string> = {
  audit_infra_errors_total: "Audit infrastructure errors",
  persist_failures_total: "Persistence failures",
  pending_records: "Pending hard-kill records",
  pending_retry_failures_total: "Pending retry failures",
  pending_oldest_age_ms: "Oldest pending age (ms)",
  lease_expired_during_deliberation_total: "Expired deliberation leases",
  duplicate_charge_alarms_total: "Duplicate charge alarms",
  directive_ttl_expired_total: "Directive TTL expirations",
  directive_max_delivery_exceeded_total: "Directive delivery exhaustion",
  directive_clock_skew_rejected_total: "Directive clock-skew rejections",
  recon_divergence_total: "Reconciliation divergence alarms",
  recon_cap_breach_total: "Reconciliation cap breaches",
  settle_ceiling_overshoot_total: "Settlement ceiling overshoots",
  spend_gauge_read_failures_total: "Spend gauge read failures",
  kill_switch_drain_timeout_total: "Kill-switch drain timeouts",
};

function DegradationPanel({ degradation }: { degradation: WatchDegradation }) {
  const rows = Object.entries(degradation) as [keyof WatchDegradation, number][];
  return (
    <section className="space-y-2">
      <header className="flex items-center gap-2 text-[10px] font-mono uppercase tracking-widest text-fg-dim">
        <ShieldCheck className="w-3.5 h-3.5" /> Narrow degradation counters
      </header>
      <div className="panel p-4 grid grid-cols-1 md:grid-cols-2 gap-x-8 gap-y-2 text-xs font-mono">
        {rows.map(([key, value]) => (
          <div key={key} className="flex justify-between gap-4">
            <span className="text-fg-muted">{DEGRADATION_LABELS[key]}</span>
            <span className={value > 0 ? "text-amber" : "text-fg-dim"}>{value}</span>
          </div>
        ))}
      </div>
    </section>
  );
}
