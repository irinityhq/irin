"use client";

import { motion } from "framer-motion";
import { useState } from "react";
import { ChevronDown, ChevronRight, Crown, Info, Scale, Sparkles, Zap } from "lucide-react";
import { cn, convergenceTone } from "@/lib/cn";
import type {
  BudgetPausedData,
  DeliberationState,
  InterventionPayload,
  RoundRuntimeState,
} from "@/lib/types";
import SeatCard from "./SeatCard";
import InterventionPanel from "./InterventionPanel";
import DivergenceScatter from "./DivergenceScatter";
import { ValidationStrip } from "./proceeding/ValidationStrip";

export default function StreamingDeliberation({
  state,
  onIntervene,
}: {
  state: DeliberationState;
  onIntervene: (p: InterventionPayload) => void;
}) {
  const current = state.rounds.find((r) => r.round_num === state.current_round);
  const prior = state.rounds.filter((r) => r.round_num < state.current_round);

  return (
    <div className="grid grid-cols-12 gap-6">
      <div className="col-span-12 lg:col-span-9 space-y-6">
        <SessionHeader state={state} />

        {state.budget_paused && (
          <BudgetBanner data={state.budget_paused} />
        )}
        {state.phase_label && state.phase !== "done" && (
          <PhaseBanner label={state.phase_label} />
        )}

        {state.specops && (
          <SpecopsBanner specops={state.specops} />
        )}

        {prior.map((r) => (
          <RoundBlock key={r.round_num} round={r} dimmed seats={state.active_seats} />
        ))}
        {current && (
          <RoundBlock round={current} seats={state.active_seats} live />
        )}

        {state.phase === "synthesizing" && (
          <SynthesizingBanner chair={state.chair} />
        )}

        {state.info_messages.length > 0 && (
          <OperatorLog messages={state.info_messages} />
        )}
      </div>

      <aside className="col-span-12 lg:col-span-3 space-y-4">
        {state.phase === "paused" && state.awaiting && (
          <InterventionPanel
            awaiting={state.awaiting}
            onIntervene={onIntervene}
            activeSeats={state.active_seats}
          />
        )}
        {state.precedent.length > 0 && (
          <PrecedentRail matches={state.precedent} />
        )}
        <SeatRoster state={state} />
      </aside>
    </div>
  );
}

function SessionHeader({ state }: { state: DeliberationState }) {
  return (
    <div className="panel-glass p-5 relative overflow-hidden scan-overlay">
      <div className="flex items-start justify-between gap-4">
        <div>
          <div className="flex items-center gap-2 mb-1">
            <span className="chip chip-amber">{state.cabinet_label}</span>
            {state.mode === "blind" && (
              <span className="chip chip-cyan">BLIND</span>
            )}
            <span className="chip">
              R {state.current_round}/{state.rounds_planned}
            </span>
            {state.tier && (
              <span className="chip chip-muted">{state.tier}</span>
            )}
            {state.stream_phase && state.stream_phases_total && state.stream_phases_total > 1 && (
              <span className="chip chip-cyan">
                Phase {state.stream_phase}/{state.stream_phases_total}
              </span>
            )}
            {state.deliberation_mode && (
              <span className="chip">{state.deliberation_mode}</span>
            )}
          </div>
          <div className="font-display text-lg text-fg-bright leading-snug max-w-3xl">
            {state.topic}
          </div>
        </div>
        <div className="text-right">
          <div className="text-[10px] uppercase tracking-widest text-fg-dim">
            Session
          </div>
          <div className="font-mono text-xs text-amber">{state.session_id}</div>
        </div>
      </div>
    </div>
  );
}

function RoundBlock({
  round,
  seats,
  dimmed,
  live,
}: {
  round: RoundRuntimeState;
  seats: { name: string; provider: string; model: string }[];
  dimmed?: boolean;
  live?: boolean;
}) {
  const tone = round.convergence != null
    ? convergenceTone(round.convergence)
    : "muted";

  return (
    <motion.section
      initial={{ opacity: 0, y: 12 }}
      animate={{ opacity: dimmed ? 0.6 : 1, y: 0 }}
      transition={{ duration: 0.4 }}
      className={cn(
        "panel relative",
        live && "border-cyan/40 animate-pulse-cyan",
      )}
    >
      <div className="flex items-center justify-between px-5 py-3 border-b border-border">
        <div className="flex items-center gap-3">
          <Scale className={cn("w-4 h-4", live ? "text-cyan" : "text-fg-muted")} />
          <span className="font-display font-bold text-fg-bright">
            Round {round.round_num}
          </span>
          {round.early_convergence && (
            <span className="chip chip-success">EARLY CONVERGENCE</span>
          )}
        </div>
        <ConvergenceMeter
          score={round.convergence}
          tone={tone === "muted" ? "warning" : tone}
        />
      </div>
      {round.validation && (
        <ValidationStrip validation={round.validation} />
      )}
      {round.divergence && round.divergence.length > 0 && (
        <div className="px-5 py-3 border-t border-border flex items-center gap-3">
          <span className="label text-fg-dim shrink-0">Divergence</span>
          <DivergenceScatter points={round.divergence} seats={seats} />
        </div>
      )}
      <div className="p-5 grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4">
        {seats.map((s) => {
          const seat = round.seats[s.name];
          if (!seat) return null;
          return <SeatCard key={s.name} seat={seat} />;
        })}
      </div>
    </motion.section>
  );
}

function ConvergenceMeter({
  score,
  tone,
}: {
  score?: number;
  tone: "success" | "warning" | "danger";
}) {
  const value = Math.round((score ?? 0) * 100);
  return (
    <div className="flex items-center gap-3">
      <span className="text-[10px] uppercase tracking-widest text-fg-dim">
        Convergence
      </span>
      <div className="w-32 h-1.5 bg-bg-deep rounded overflow-hidden">
        <motion.div
          initial={{ width: 0 }}
          animate={{ width: `${value}%` }}
          transition={{ duration: 0.6 }}
          className={cn(
            "h-full",
            tone === "success" && "bg-success",
            tone === "warning" && "bg-warning",
            tone === "danger" && "bg-danger",
          )}
        />
      </div>
      <span className={cn("font-mono text-sm tabular-nums",
        tone === "success" && "text-success",
        tone === "warning" && "text-warning",
        tone === "danger" && "text-danger",
      )}>
        {score == null ? "—" : `${value}%`}
      </span>
    </div>
  );
}

function SpecopsBanner({
  specops,
}: {
  specops: NonNullable<DeliberationState["specops"]>;
}) {
  return (
    <motion.div
      initial={{ opacity: 0, scale: 0.98 }}
      animate={{ opacity: 1, scale: 1 }}
      className="panel border-magenta/40 bg-magenta/5 p-5 relative overflow-hidden animate-pulse-magenta"
    >
      <div className="flex items-center gap-2 mb-2">
        <Zap className="w-4 h-4 text-magenta" />
        <span className="label text-magenta">SpecOps Signal</span>
        <span className="chip chip-magenta">{specops.model}</span>
        {specops.trigger && (
          <span className="chip">{specops.trigger}</span>
        )}
      </div>
      <div className="text-fg leading-relaxed">{specops.text}</div>
    </motion.div>
  );
}

function SynthesizingBanner({
  chair,
}: {
  chair: { provider: string; model: string };
}) {
  return (
    <div className="panel border-amber/40 bg-amber/5 p-5 animate-pulse-amber">
      <div className="flex items-center gap-3">
        <Crown className="w-5 h-5 text-amber" />
        <div>
          <div className="font-display font-bold text-amber">
            Chair synthesizing…
          </div>
          <div className="text-xs font-mono text-fg-dim">
            {chair.provider} · {chair.model} · thinking effort: high
          </div>
        </div>
      </div>
    </div>
  );
}

function PrecedentRail({
  matches,
}: {
  matches: DeliberationState["precedent"];
}) {
  return (
    <div className="panel p-4">
      <div className="flex items-center gap-2 mb-3">
        <Sparkles className="w-3.5 h-3.5 text-amber" />
        <span className="label">Precedent Loaded</span>
      </div>
      <div className="space-y-2 max-h-64 overflow-y-auto pr-1">
        {matches.slice(0, 5).map((m) => (
          <div
            key={m.id}
            className="text-xs font-mono border-l-2 border-amber/40 pl-2 py-1"
          >
            <div className="text-amber/80 flex items-center gap-1">
              <span>{m.id}</span>
              {m.score != null && (
                <span className="ml-auto text-cyan tabular-nums" title={m.why}>
                  {Math.round(m.score * 100)}%
                </span>
              )}
            </div>
            <div className="text-fg-muted line-clamp-2">{m.topic}</div>
            <div className="text-fg-dim mt-0.5">
              {m.cabinet} · {m.confidence} · {m.ts.slice(0, 10)}
              {m.why ? ` · ${m.why}` : ""}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function SeatRoster({ state }: { state: DeliberationState }) {
  return (
    <div className="panel p-4">
      <div className="label mb-3">Seats</div>
      <div className="space-y-2">
        {state.active_seats.map((s) => (
          <div
            key={s.name}
            className="flex items-center justify-between text-xs font-mono"
          >
            <span className="text-fg">{s.name}</span>
            <span className="text-fg-dim">{s.provider}</span>
          </div>
        ))}
        {state.dropped_seats.length > 0 && (
          <>
            <div className="border-t border-border pt-2 mt-2 label text-danger">
              Missing
            </div>
            {state.dropped_seats.map((s) => (
              <div
                key={s.name}
                className="flex items-center justify-between text-xs font-mono opacity-50"
              >
                <span className="line-through">{s.name}</span>
                <span>{s.provider}</span>
              </div>
            ))}
          </>
        )}
        <div className="flex items-center justify-between text-xs font-mono pt-2 border-t border-border">
          <span className="text-amber">Chair</span>
          <span className="text-fg-dim">{state.chair.provider}</span>
        </div>
      </div>
    </div>
  );
}

function OperatorLog({
  messages,
}: {
  messages: DeliberationState["info_messages"];
}) {
  const [expanded, setExpanded] = useState(false);
  const latest = messages[messages.length - 1];

  return (
    <div className="panel border-border">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="w-full flex items-center gap-2 px-5 py-3 text-xs font-mono hover:bg-bg-overlay transition-colors"
      >
        <Info className="w-3.5 h-3.5 text-cyan shrink-0" />
        <span className="label text-cyan">Operator log</span>
        <span className="chip text-[10px]">{messages.length}</span>
        {!expanded && latest && (
          <span className="text-fg-dim truncate ml-1 flex-1 text-left">
            {latest.message}
          </span>
        )}
        <span className="text-fg-dim ml-auto shrink-0">
          {expanded ? <ChevronDown className="w-3 h-3" /> : <ChevronRight className="w-3 h-3" />}
        </span>
      </button>
      {expanded && (
        <div className="px-5 pb-4 space-y-2 max-h-48 overflow-y-auto border-t border-border">
          {messages.map((m, i) => (
            <div key={`${m.ts}-${i}`} className="text-xs font-mono border-l-2 border-cyan/40 pl-3 py-1">
              <div className="text-fg-dim text-[10px]">{m.ts}</div>
              <div className="text-fg leading-snug">{m.message}</div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export { ValidationStrip } from "./proceeding/ValidationStrip";

function BudgetBanner({ data }: { data: BudgetPausedData }) {
  return (
    <div className="panel border-amber/50 bg-amber/10 p-4 flex items-center gap-2 text-sm">
      <Scale className="w-4 h-4 text-amber shrink-0" />
      <span>
        <strong className="text-amber">Budget pause</strong>
        {" — "}
        ${data.total_cost_usd.toFixed(4)} reached cap ${data.max_usd.toFixed(2)} after round{" "}
        {data.round_num}; ending early.
      </span>
    </div>
  );
}

function PhaseBanner({ label }: { label: string }) {
  return (
    <div className="panel border-cyan/40 bg-cyan/5 p-4 text-sm font-mono text-cyan">
      {label}
    </div>
  );
}
