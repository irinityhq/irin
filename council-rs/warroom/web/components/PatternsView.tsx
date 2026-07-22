"use client";

import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import {
  Activity, BarChart3, Brain, Compass, Crosshair, Flame, Hash, MessagesSquare,
  Pause, Scissors, StopCircle, TrendingUp,
} from "lucide-react";
import { api } from "@/lib/api";
import { cn, convergenceTone } from "@/lib/cn";
import type { InterventionEntry, PatternsResponse } from "@/lib/types";

const ACTION_META: Record<string, { tone: string; icon: React.ReactNode; label: string }> = {
  continue: { tone: "cyan", icon: <Activity className="w-3.5 h-3.5" />, label: "Continue" },
  end_early: { tone: "amber", icon: <StopCircle className="w-3.5 h-3.5" />, label: "End early" },
  escalate_specops: { tone: "magenta", icon: <Crosshair className="w-3.5 h-3.5" />, label: "SpecOps" },
  escalate_munger: { tone: "amber", icon: <Brain className="w-3.5 h-3.5" />, label: "Munger" },
  escalate_contrarian: { tone: "magenta", icon: <Flame className="w-3.5 h-3.5" />, label: "Contrarian" },
  escalate_kiss: { tone: "cyan", icon: <Scissors className="w-3.5 h-3.5" />, label: "KISS" },
  inject_context: { tone: "warning", icon: <MessagesSquare className="w-3.5 h-3.5" />, label: "Inject context" },
  swap_seat: { tone: "warning", icon: <Compass className="w-3.5 h-3.5" />, label: "Swap seat" },
  unknown: { tone: "muted", icon: <Pause className="w-3.5 h-3.5" />, label: "Unknown" },
};

const LOG_PAGE_SIZE = 50;

export default function PatternsView() {
  const [days, setDays] = useState<number | undefined>(30);
  const [data, setData] = useState<PatternsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [patternsError, setPatternsError] = useState(false);
  const [patternsRetry, setPatternsRetry] = useState(0);

  // Intervention log (dedicated /api/interventions endpoint)
  const [logEntries, setLogEntries] = useState<InterventionEntry[]>([]);
  const [logTotal, setLogTotal] = useState(0);
  const [logLimit, setLogLimit] = useState(LOG_PAGE_SIZE);
  const [logLoading, setLogLoading] = useState(false);
  const [logError, setLogError] = useState(false);
  const [logRetry, setLogRetry] = useState(0);
  const [copiedId, setCopiedId] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setData(null);
    setPatternsError(false);
    api.patterns(days)
      .then((result) => {
        if (active) setData(result);
      })
      .catch(() => {
        if (active) setPatternsError(true);
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [days, patternsRetry]);

  // Reset paging when window changes
  useEffect(() => {
    setLogLimit(LOG_PAGE_SIZE);
    setLogEntries([]);
    setLogTotal(0);
    setLogError(false);
  }, [days]);

  useEffect(() => {
    let active = true;
    setLogLoading(true);
    setLogError(false);
    api.interventions(days, logLimit)
      .then((r) => {
        if (active) {
          setLogEntries(r.entries);
          setLogTotal(r.total);
        }
      })
      .catch(() => {
        if (active) setLogError(true);
      })
      .finally(() => {
        if (active) setLogLoading(false);
      });
    return () => {
      active = false;
    };
  }, [days, logLimit, logRetry]);

  const copySessionId = (id: string) => {
    if (typeof navigator !== "undefined" && navigator.clipboard) {
      navigator.clipboard.writeText(id).catch(() => {});
    }
    setCopiedId(id);
    setTimeout(() => setCopiedId((c) => (c === id ? null : c)), 1200);
  };

  if (loading) {
    return (
      <div className="rounded border border-border bg-bg-elevated p-12 text-center font-mono text-sm text-fg-dim">
        Loading patterns…
      </div>
    );
  }

  if (patternsError || !data) {
    return (
      <div
        role="alert"
        className="space-y-3 rounded border border-danger/40 bg-bg-elevated p-12 text-center"
      >
        <div className="text-sm font-semibold text-danger">
          Could not load operator patterns
        </div>
        <div className="text-xs text-fg-muted">
          The Council patterns endpoint did not respond successfully.
        </div>
        <button
          type="button"
          className="btn text-xs"
          onClick={() => setPatternsRetry((n) => n + 1)}
        >
          Retry
        </button>
      </div>
    );
  }

  if (data.total === 0) {
    return (
      <div className="space-y-3 rounded border border-border bg-bg-elevated p-12 text-center">
        <div className="text-base font-semibold tracking-tight text-fg">
          No interventions yet
        </div>
        <div className="mx-auto max-w-md text-sm leading-relaxed text-fg-muted">
          Run a deliberation with pause-after-each-round enabled, then come
          back. Every escalation, context injection, and continue gets logged.
        </div>
      </div>
    );
  }

  const totalActions = Object.values(data.actions).reduce((a, b) => a + b, 0);
  const convAtPause = Math.round(data.avg_convergence_at_pause * 100);

  return (
    <div className="space-y-5">
      <header className="flex items-start justify-between gap-4 border-b border-border pb-3">
        <div className="min-w-0">
          <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
            Operator telemetry
          </div>
          <h1 className="mt-1 text-base font-semibold tracking-tight text-fg">
            Operator Patterns
          </h1>
          <p className="mt-1 max-w-xl text-[11px] leading-relaxed text-fg-muted">
            How you actually steer the council.
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <span className="label">Window</span>
          {[7, 30, 90, undefined].map((d) => (
            <button
              key={String(d)}
              onClick={() => setDays(d)}
              className={cn("btn text-xs", days === d && "btn-primary")}
            >
              {d ? `${d}d` : "all"}
            </button>
          ))}
        </div>
      </header>

      <div className="flex rounded border border-border bg-bg-elevated">
        <Metric label="Interventions" value={data.total} accent="amber" />
        <Metric label="Sessions touched" value={data.session_count} />
        <Metric
          label="Avg conv at pause"
          value={`${convAtPause}%`}
          accent={convergenceTone(data.avg_convergence_at_pause)}
        />
        <Metric label="Multi-step" value={data.multi_intervention_sessions} />
      </div>

      <div className="grid grid-cols-1 gap-5 lg:grid-cols-2">
        <Panel title="Action breakdown" icon={<BarChart3 className="w-3.5 h-3.5" />}>
          <div className="space-y-2">
            {Object.entries(data.actions)
              .sort(([, a], [, b]) => b - a)
              .map(([action, count]) => {
                const meta = ACTION_META[action] || ACTION_META.unknown;
                const pct = Math.round((count / totalActions) * 100);
                return (
                  <div key={action} className="space-y-1">
                    <div className="flex items-center justify-between text-xs">
                      <div className="flex items-center gap-2">
                        <span className={cn(`text-${meta.tone}`)}>{meta.icon}</span>
                        <span className="font-mono text-fg-muted">{meta.label}</span>
                      </div>
                      <span className="font-mono tabular-nums text-fg-dim">
                        {count} · {pct}%
                      </span>
                    </div>
                    <div className="h-1 overflow-hidden rounded-sm bg-bg-deep">
                      <motion.div
                        initial={{ width: 0 }}
                        animate={{ width: `${pct}%` }}
                        transition={{ duration: 0.6 }}
                        className={cn(
                          "h-full",
                          meta.tone === "amber" && "bg-amber",
                          meta.tone === "cyan" && "bg-cyan",
                          meta.tone === "magenta" && "bg-magenta",
                          meta.tone === "success" && "bg-success",
                          meta.tone === "warning" && "bg-warning",
                          meta.tone === "muted" && "bg-fg-dim",
                        )}
                      />
                    </div>
                  </div>
                );
              })}
          </div>
        </Panel>

        <Panel title="Convergence at intervention" icon={<TrendingUp className="w-3.5 h-3.5" />}>
          <div className="space-y-2">
            {Object.entries(data.convergence_buckets).map(([bucket, count]) => {
              const total = Object.values(data.convergence_buckets).reduce(
                (a, b) => a + b, 0);
              const pct = total ? Math.round((count / total) * 100) : 0;
              const tone =
                bucket === "0-20%" || bucket === "20-40%"
                  ? "danger"
                  : bucket === "40-60%"
                    ? "warning"
                    : "success";
              return (
                <div key={bucket} className="flex items-center gap-3 text-xs">
                  <span className="w-16 font-mono text-fg-dim">{bucket}</span>
                  <div className="h-1 flex-1 overflow-hidden rounded-sm bg-bg-deep">
                    <motion.div
                      initial={{ width: 0 }}
                      animate={{ width: `${pct}%` }}
                      className={cn(
                        "h-full",
                        tone === "danger" && "bg-danger",
                        tone === "warning" && "bg-warning",
                        tone === "success" && "bg-success",
                      )}
                    />
                  </div>
                  <span className="w-10 text-right font-mono tabular-nums text-fg-muted">
                    {count}
                  </span>
                </div>
              );
            })}
          </div>
          <div className="mt-3 border-t border-border pt-2 text-[10px] font-mono leading-relaxed text-fg-dim">
            Lower buckets = you intervene when the council is most divided.
            Higher buckets = you intervene even when consensus is forming.
          </div>
        </Panel>

        <Panel title="By cabinet" icon={<Hash className="w-3.5 h-3.5" />}>
          <div className="max-h-72 space-y-2.5 overflow-y-auto">
            {Object.entries(data.by_cabinet).map(([cab, actions]) => {
              const t = Object.values(actions).reduce((a, b) => a + b, 0);
              return (
                <div key={cab}>
                  <div className="mb-1 flex items-center justify-between text-xs font-mono">
                    <span className="text-fg-muted">{cab}</span>
                    <span className="tabular-nums text-fg-dim">{t}</span>
                  </div>
                  <div className="flex h-1 gap-px overflow-hidden rounded-sm bg-bg-deep">
                    {Object.entries(actions).map(([action, count]) => {
                      const meta = ACTION_META[action] || ACTION_META.unknown;
                      const w = (count / t) * 100;
                      return (
                        <div
                          key={action}
                          style={{ width: `${w}%` }}
                          title={`${meta.label}: ${count}`}
                          className={cn(
                            meta.tone === "amber" && "bg-amber",
                            meta.tone === "cyan" && "bg-cyan",
                            meta.tone === "magenta" && "bg-magenta",
                            meta.tone === "success" && "bg-success",
                            meta.tone === "warning" && "bg-warning",
                            meta.tone === "muted" && "bg-fg-dim",
                          )}
                        />
                      );
                    })}
                  </div>
                </div>
              );
            })}
          </div>
        </Panel>

        <Panel title="Top topics that pull intervention" icon={<MessagesSquare className="w-3.5 h-3.5" />}>
          <div className="rounded border border-border bg-bg-elevated">
            {data.top_keywords.slice(0, 20).map(([kw, count]) => (
              <div
                key={kw}
                className="flex items-center justify-between gap-3 border-b border-border px-3 py-1 text-[11px] font-mono last:border-b-0"
              >
                <span className="truncate text-fg-muted">{kw}</span>
                <span className={cn(
                  "tabular-nums",
                  count > 5 ? "text-amber" : "text-fg-dim",
                )}>
                  {count}
                </span>
              </div>
            ))}
          </div>
        </Panel>
      </div>

      {Object.keys(data.by_round).length > 0 && (
        <Panel title="By round" icon={<BarChart3 className="w-3.5 h-3.5" />}>
          <div className="space-y-2">
            {Object.entries(data.by_round)
              .sort(([a], [b]) => Number(a) - Number(b))
              .map(([round, count]) => {
                const pct = Math.round((count / data.total) * 100);
                return (
                  <div key={round} className="flex items-center gap-3 text-xs">
                    <span className="w-10 font-mono text-fg-dim">R{round}</span>
                    <div className="h-1 flex-1 overflow-hidden rounded-sm bg-bg-deep">
                      <motion.div
                        initial={{ width: 0 }}
                        animate={{ width: `${pct}%` }}
                        className="h-full bg-amber"
                      />
                    </div>
                    <span className="w-10 text-right font-mono tabular-nums text-fg-muted">
                      {count}
                    </span>
                  </div>
                );
              })}
          </div>
        </Panel>
      )}

      {data.sequences.length > 0 && (
        <Panel title="Intervention sequences" icon={<TrendingUp className="w-3.5 h-3.5" />}>
          <div className="max-h-48 space-y-2 overflow-y-auto">
            {data.sequences.slice(0, 10).map((seq, i) => (
              <div key={i} className="flex flex-wrap items-center gap-1">
                {seq.map((action, j) => {
                  const meta = ACTION_META[action] || ACTION_META.unknown;
                  return (
                    <span key={j} className="flex items-center gap-1">
                      {j > 0 && <span className="text-fg-dim text-[10px]">→</span>}
                      <span className={cn("chip text-[10px]", `chip-${meta.tone}`)}>
                        {meta.label}
                      </span>
                    </span>
                  );
                })}
              </div>
            ))}
          </div>
        </Panel>
      )}

      <Panel
        title={`Intervention log${logTotal ? ` · ${logEntries.length} of ${logTotal}` : ""}`}
        icon={<Activity className="w-3.5 h-3.5" />}
      >
        {logError && (
          <div
            role="alert"
            className="mb-3 flex items-center justify-between gap-3 rounded border border-danger/40 px-3 py-2"
          >
            <span className="text-xs text-danger">
              Could not load intervention log.
            </span>
            <button
              type="button"
              className="btn text-xs"
              onClick={() => setLogRetry((n) => n + 1)}
            >
              Retry
            </button>
          </div>
        )}
        {logLoading && logEntries.length === 0 ? (
          <div className="py-6 text-center text-xs font-mono text-fg-dim">
            Loading interventions…
          </div>
        ) : !logError && logEntries.length === 0 ? (
          <div className="py-6 text-center text-xs font-mono text-fg-dim">
            No interventions in window.
          </div>
        ) : logEntries.length > 0 ? (
          <>
            <div className="max-h-[28rem] overflow-y-auto rounded border border-border bg-bg-elevated">
              {logEntries.map((e, i) => (
                <InterventionRow
                  key={`${e.session_id}-${e.round_num}-${e.ts}-${i}`}
                  entry={e}
                  copiedId={copiedId}
                  onCopy={copySessionId}
                />
              ))}
            </div>
            {logEntries.length < logTotal && (
              <div className="mt-3 flex items-center justify-between border-t border-border pt-3">
                <span className="text-[10px] font-mono text-fg-dim">
                  {logTotal - logEntries.length} more
                </span>
                <button
                  onClick={() => setLogLimit((n) => n + LOG_PAGE_SIZE)}
                  disabled={logLoading}
                  className={cn("btn text-xs", logLoading && "opacity-50")}
                >
                  {logLoading ? "Loading…" : "Load more"}
                </button>
              </div>
            )}
          </>
        ) : null}
      </Panel>
    </div>
  );
}

function InterventionRow({
  entry, copiedId, onCopy,
}: {
  entry: InterventionEntry;
  copiedId: string | null;
  onCopy: (id: string) => void;
}) {
  const meta = ACTION_META[entry.action] || ACTION_META.unknown;
  const conv = Math.round(entry.convergence_at_pause * 100);
  const convTone =
    conv < 40 ? "text-danger"
    : conv < 60 ? "text-warning"
    : "text-success";
  const date = entry.ts.slice(0, 10);
  const time = entry.ts.slice(11, 19);
  const detail = describePayload(entry);

  return (
    <div className="border-b border-border px-3 py-1.5 transition-colors last:border-b-0 hover:bg-bg-overlay/40">
      <div className="flex flex-wrap items-center gap-3">
        <span className={cn(`text-${meta.tone} shrink-0`)}>{meta.icon}</span>
        <span className={cn("chip text-[10px] shrink-0", `chip-${meta.tone}`)}>
          {meta.label}
        </span>
        <button
          onClick={() => onCopy(entry.session_id)}
          title="Copy session ID"
          className="shrink-0 cursor-pointer font-mono text-[10px] text-amber hover:text-amber/80"
        >
          {copiedId === entry.session_id ? "copied" : entry.session_id}
        </button>
        <span className="shrink-0 font-mono text-[10px] text-fg-dim tabular-nums">
          R{entry.round_num}
        </span>
        <span className={cn(
          "shrink-0 font-mono text-[10px] tabular-nums",
          convTone,
        )}>
          {conv}%
        </span>
        <span className="ml-auto shrink-0 font-mono text-[10px] text-fg-dim tabular-nums">
          {date} {time}
        </span>
      </div>
      {detail && (
        <div className="mt-1 ml-6 break-words font-mono text-[10px] text-fg-muted">
          {detail}
        </div>
      )}
    </div>
  );
}

function describePayload(e: InterventionEntry): string | null {
  const p = e.payload || {};
  if (e.action === "inject_context") {
    const text = typeof p.text === "string" ? p.text : "";
    if (!text) return null;
    const trimmed = text.replace(/\s+/g, " ").trim();
    const preview = trimmed.length > 180
      ? trimmed.slice(0, 180) + "…"
      : trimmed;
    return `“${preview}”`;
  }
  if (e.action === "swap_seat") {
    const seat = typeof p.seat_name === "string" ? p.seat_name : null;
    const provider = typeof p.provider === "string" ? p.provider : null;
    const model = typeof p.model === "string" ? p.model : null;
    const parts: string[] = [];
    if (seat) parts.push(`seat: ${seat}`);
    if (provider || model) {
      parts.push(`→ ${[provider, model].filter(Boolean).join("/")}`);
    }
    if (typeof p.system === "string" && p.system) {
      const sys = p.system.replace(/\s+/g, " ").trim();
      parts.push(`system: ${sys.length > 80 ? sys.slice(0, 80) + "…" : sys}`);
    }
    return parts.length ? parts.join(" · ") : null;
  }
  // Generic: stringify any non-empty payload keys for visibility
  const keys = Object.keys(p);
  if (keys.length === 0) return null;
  const parts = keys.slice(0, 3).map((k) => {
    const v = p[k];
    if (typeof v === "string") {
      const s = v.replace(/\s+/g, " ").trim();
      return `${k}: ${s.length > 60 ? s.slice(0, 60) + "…" : s}`;
    }
    if (typeof v === "number" || typeof v === "boolean") return `${k}: ${v}`;
    return null;
  }).filter(Boolean);
  return parts.length ? parts.join(" · ") : null;
}

function Metric({
  label, value, accent,
}: {
  label: string;
  value: number | string;
  accent?: "amber" | "success" | "warning" | "danger";
}) {
  return (
    <div className="flex flex-1 flex-col justify-center gap-0.5 border-r border-border px-3 py-2.5 last:border-r-0">
      <span className="text-[9px] font-mono uppercase tracking-wide text-fg-dim">
        {label}
      </span>
      <b
        className={cn(
          "text-lg font-semibold tabular-nums",
          accent === "amber" && "text-amber",
          accent === "success" && "text-success",
          accent === "warning" && "text-warning",
          accent === "danger" && "text-danger",
          !accent && "text-fg",
        )}
      >
        {value}
      </b>
    </div>
  );
}

function Panel({
  title, icon, children,
}: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="rounded border border-border bg-bg-elevated p-4">
      <div className="cg-section-label">
        <span className="text-fg-dim">{icon}</span>
        {title}
      </div>
      {children}
    </div>
  );
}
