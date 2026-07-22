"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { motion } from "framer-motion";
import {
  AlertTriangle, Compass, ExternalLink, Loader2, Play, X,
} from "lucide-react";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import type { WeeklySummary } from "@/lib/types";
import { useToast } from "./Toast";

const DISMISS_KEY_PREFIX = "council:weekly_dismissed:";

type Tone = "danger" | "warning" | "success" | undefined;

export default function WeeklyDriftCard({
  onViewReport,
}: {
  onViewReport?: (reportFilename: string) => void;
}) {
  const [summary, setSummary] = useState<WeeklySummary | null>(null);
  const [running, setRunning] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [postWebhooks, setPostWebhooks] = useState(false);
  const initialTsRef = useRef<string | null>(null);
  const { toast } = useToast();

  const fetchSummary = useCallback(async () => {
    try {
      const s = await api.weeklyLatest();
      setSummary(s);
      if (typeof window !== "undefined") {
        const key = DISMISS_KEY_PREFIX + s.ts;
        setDismissed(!!localStorage.getItem(key));
      }
      return s;
    } catch {
      setSummary(null);
      return null;
    }
  }, []);

  useEffect(() => {
    fetchSummary().then((s) => {
      if (s) initialTsRef.current = s.ts;
    });
  }, [fetchSummary]);

  // While running, poll for a fresh summary.
  useEffect(() => {
    if (!running) return;
    const id = setInterval(async () => {
      const s = await fetchSummary();
      if (s && s.ts !== initialTsRef.current) {
        setRunning(false);
        initialTsRef.current = s.ts;
      }
    }, 4000);
    return () => clearInterval(id);
  }, [running, fetchSummary]);

  if (!summary || dismissed) return null;
  if (summary.sessions_analyzed === 0 && !summary.error) {
    // Nothing to report this week — quietly skip the card.
    return null;
  }

  const drift = summary.avg_drift;
  const tone: Exclude<Tone, undefined> =
    drift > 0.4 ? "danger" : drift > 0.2 ? "warning" : "success";

  const dismiss = () => {
    if (typeof window !== "undefined") {
      localStorage.setItem(DISMISS_KEY_PREFIX + summary.ts, "1");
    }
    setDismissed(true);
  };

  const rerun = async () => {
    setRunning(true);
    try {
      await api.weeklyRun(7, 8, postWebhooks);
    } catch {
      setRunning(false);
      toast("error", "Weekly drift run failed");
    }
  };

  return (
    <motion.div
      key={summary.ts}
      initial={{ opacity: 0, y: -6 }}
      animate={{ opacity: 1, y: 0 }}
      className={cn(
        "rounded border border-l-2 border-border bg-bg-elevated",
        tone === "danger" && "border-l-danger",
        tone === "warning" && "border-l-warning",
        tone === "success" && "border-l-success",
      )}
    >
      <div className="flex items-start justify-between gap-3 border-b border-border px-4 py-3">
        <div className="flex items-center gap-2.5 min-w-0">
          <Compass className="w-4 h-4 shrink-0 text-fg-dim" />
          <div className="min-w-0">
            <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
              Weekly drift summary
            </div>
            <div className="mt-0.5 text-[10px] font-mono text-fg-dim tabular-nums">
              {new Date(summary.ts).toLocaleString()} · {summary.window_days}d window
            </div>
          </div>
        </div>
        <button
          onClick={dismiss}
          className="shrink-0 p-1 text-fg-dim transition-colors hover:text-fg"
          title="Dismiss until next run"
        >
          <X className="w-4 h-4" />
        </button>
      </div>

      <div className="p-4 space-y-4">
        <div className="flex rounded border border-border bg-bg-elevated">
          <Metric label="Sessions" value={String(summary.sessions_analyzed)} />
          <Metric label="Avg drift" value={drift.toFixed(3)} tone={tone} hero />
          <Metric
            label="Confidence flips"
            value={String(summary.confidence_flips)}
            tone={summary.confidence_flips > 0 ? "warning" : undefined}
          />
          <Metric
            label="High-drift"
            value={String(summary.high_drift_count)}
            tone={summary.high_drift_count > 0 ? "warning" : undefined}
          />
        </div>

        {summary.top_anchoring && summary.top_anchoring.length > 0 && (
          <div>
            <div className="cg-section-label">Top anchoring patterns</div>
            <div className="rounded border border-border bg-bg-elevated">
              {summary.top_anchoring.map((p) => {
                const t = driftTone(p.avg_drift);
                return (
                  <div
                    key={p.keyword}
                    className="grid grid-cols-[1fr_auto_auto] items-center gap-3 border-b border-border px-3 py-1.5 text-[11px] font-mono last:border-b-0"
                    title={`avg drift ${p.avg_drift.toFixed(2)} across ${p.session_count} session${p.session_count > 1 ? "s" : ""}`}
                  >
                    <span className="truncate text-fg-muted">{p.keyword}</span>
                    <span className={cn(
                      "tabular-nums text-right",
                      t === "danger" && "text-danger",
                      t === "warning" && "text-warning",
                      t === "success" && "text-success",
                    )}>
                      {p.avg_drift.toFixed(2)}
                    </span>
                    <span className="w-8 text-right tabular-nums text-fg-dim">
                      ×{p.session_count}
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        {summary.headline_session && (
          <div className="border-l-2 border-amber/50 pl-3 py-1">
            <div className="flex flex-wrap items-center gap-2 mb-1">
              <AlertTriangle className="w-3.5 h-3.5 text-amber shrink-0" />
              <span className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
                Highest-drift session
              </span>
              <span className="font-mono text-[10px] text-amber">
                {summary.headline_session.session_id}
              </span>
              <span className="font-mono text-[10px] text-fg-dim tabular-nums">
                drift {summary.headline_session.drift_score.toFixed(3)}
              </span>
              {summary.headline_session.confidence_changed && (
                <span className="font-mono text-[10px] text-warning tabular-nums">
                  {summary.headline_session.confidence_normal} →{" "}
                  {summary.headline_session.confidence_blind}
                </span>
              )}
            </div>
            <div className="text-xs text-fg-muted line-clamp-2">
              {summary.headline_session.topic}
            </div>
          </div>
        )}

        <div className="flex items-center gap-3">
          <label className="flex items-center gap-1.5 text-[10px] font-mono text-fg-dim cursor-pointer">
            <input
              type="checkbox"
              checked={postWebhooks}
              onChange={(e) => setPostWebhooks(e.target.checked)}
              className="w-3 h-3"
            />
            webhooks
          </label>
          <button
            onClick={rerun}
            disabled={running}
            className="btn btn-primary text-xs"
          >
            {running ? (
              <><Loader2 className="w-3.5 h-3.5 animate-spin" /> Running…</>
            ) : (
              <><Play className="w-3.5 h-3.5" /> Re-run this week&apos;s drift</>
            )}
          </button>
          {summary.report_filename && onViewReport && (
            <button
              onClick={() => onViewReport(summary.report_filename!)}
              className="btn text-xs"
            >
              <ExternalLink className="w-3.5 h-3.5" /> View full report
            </button>
          )}
          {summary.webhooks && Object.values(summary.webhooks).some(
            (s) => s === "ok" || s.startsWith("ok"),
          ) && (
            <span className="ml-auto text-[10px] font-mono text-fg-dim">
              pushed to{" "}
              {Object.entries(summary.webhooks)
                .filter(([, v]) => v.startsWith("ok"))
                .map(([k]) => k)
                .join(", ")}
            </span>
          )}
        </div>
      </div>
    </motion.div>
  );
}

function driftTone(v: number): "danger" | "warning" | "success" {
  return v > 0.4 ? "danger" : v > 0.2 ? "warning" : "success";
}

function Metric({
  label, value, tone, hero,
}: {
  label: string;
  value: string;
  tone?: Tone;
  hero?: boolean;
}) {
  return (
    <div className="flex flex-1 flex-col justify-center gap-0.5 border-r border-border px-3 py-2.5 last:border-r-0">
      <span className="text-[9px] font-mono uppercase tracking-wide text-fg-dim">
        {label}
      </span>
      <b
        className={cn(
          "font-semibold tabular-nums",
          hero ? "text-2xl" : "text-lg",
          tone === "danger" && "text-danger",
          tone === "warning" && "text-warning",
          tone === "success" && "text-success",
          !tone && "text-fg",
        )}
      >
        {value}
      </b>
    </div>
  );
}
