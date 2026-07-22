"use client";

import { motion } from "framer-motion";
import { cn, convergenceTone, fmtCost, fmtLatency, fmtTokens, providerHex } from "@/lib/cn";
import type { DeliberationState, DivergencePoint, SeatRef } from "@/lib/types";

export default function LiveAnalytics({ state }: { state: DeliberationState }) {
  const lastRound = state.rounds[state.rounds.length - 1];
  const conv = lastRound?.convergence;
  const tone = conv == null ? "warning" : convergenceTone(conv);
  // N02 — latest round carrying a divergence projection (events may lag).
  const divergencePoints = [...state.rounds]
    .reverse()
    .find((r) => r.divergence && r.divergence.length > 0)?.divergence;

  return (
    <motion.div
      initial={{ y: 80 }}
      animate={{ y: 0 }}
      className="sticky bottom-0 z-30 border-t border-border bg-bg-deep"
    >
      <div className="max-w-[1600px] w-full mx-auto px-6 h-12 flex items-center justify-between font-mono text-xs">
        <div className="flex items-center gap-6">
          <Stat label="Tokens" value={fmtTokens(state.totals.tokens)} />
          <Stat label="Cost" value={fmtCost(state.totals.cost_usd)} />
          <Stat label="Latency" value={fmtLatency(state.totals.latency_ms)} />
        </div>

        <div className="flex items-center gap-4">
          {divergencePoints && divergencePoints.length > 0 && (
            <div
              data-testid="live-divergence"
              className="flex items-center gap-2"
              title="Latest round seat divergence (PCA projection)"
            >
              <span className="text-fg-dim">Divergence</span>
              <DivergenceSpark points={divergencePoints} seats={state.active_seats} />
            </div>
          )}
          <div className="flex items-center gap-2">
            <span
              className={cn(
                "w-[5px] h-[5px] rounded-full shrink-0",
                tone === "success" && "bg-success",
                tone === "warning" && "bg-warning",
                tone === "danger" && "bg-danger",
              )}
            />
            <span className="text-[10px] uppercase tracking-widest text-fg-dim">
              Convergence
            </span>
            <ConvergenceTrace state={state} />
          </div>
        </div>
      </div>
    </motion.div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="text-[10px] uppercase tracking-widest text-fg-dim">{label}</span>
      <span className="text-fg tabular-nums">{value}</span>
    </div>
  );
}

/** Compact latest-round divergence scatter for the bottom analytics bar. */
function DivergenceSpark({
  points,
  seats,
}: {
  points: DivergencePoint[];
  seats: SeatRef[];
}) {
  const size = 28;
  const pad = 3;
  const inner = size - pad * 2;
  const xs = points.map((p) => p.x);
  const ys = points.map((p) => p.y);
  const minX = Math.min(...xs);
  const minY = Math.min(...ys);
  const spanX = Math.max(...xs) - minX || 1;
  const spanY = Math.max(...ys) - minY || 1;
  const provFor = (n: string) => seats.find((s) => s.name === n)?.provider ?? "";
  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      className="rounded border border-border bg-bg-deep/60"
      role="img"
      aria-label="Latest round divergence"
    >
      {points.map((p) => (
        <circle
          key={p.seat}
          cx={pad + ((p.x - minX) / spanX) * inner}
          cy={pad + (1 - (p.y - minY) / spanY) * inner}
          r={2}
          style={{ fill: providerHex(provFor(p.seat)) }}
        >
          <title>{p.seat}</title>
        </circle>
      ))}
    </svg>
  );
}

function ConvergenceTrace({ state }: { state: DeliberationState }) {
  return (
    <div className="flex items-center gap-1">
      {Array.from({ length: state.rounds_planned || 1 }).map((_, i) => {
        const r = state.rounds.find((x) => x.round_num === i + 1);
        const v = r?.convergence;
        const t = v == null ? null : convergenceTone(v);
        return (
          <div
            key={i}
            className={cn(
              "w-6 h-1.5 rounded-full",
              t == null && "bg-bg-overlay",
              t === "success" && "bg-success",
              t === "warning" && "bg-warning",
              t === "danger" && "bg-danger",
            )}
            title={v == null ? "" : `R${i + 1}: ${Math.round(v * 100)}%`}
          />
        );
      })}
    </div>
  );
}
