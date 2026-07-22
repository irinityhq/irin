"use client";

import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import {
  Brain, Crosshair, FastForward, Flame, Lightbulb, Loader2, MessagesSquare, Pause, Play,
  Scissors, Shuffle, StopCircle,
} from "lucide-react";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import { predictHint } from "@/lib/intervention-predict";
import type {
  AwaitingInputData, InterventionPayload, InterventionPrediction, SeatRef,
} from "@/lib/types";

const ESCALATION_HINTS: Partial<Record<InterventionPayload["action"], string>> = {
  escalate_specops: "Adversarial signal",
  escalate_munger: "Invert assumptions",
  escalate_contrarian: "Tear down framing",
  escalate_kiss: "Strip to essentials",
};

function pendingLabel(action: InterventionPayload["action"]): string {
  switch (action) {
    case "escalate_kiss":
      return "Applying KISS…";
    case "escalate_specops":
      return "Running SpecOps…";
    case "escalate_munger":
      return "Running Munger…";
    case "escalate_contrarian":
      return "Running Contrarian…";
    case "inject_context":
      return "Injecting note…";
    case "swap_seat":
      return "Swapping seat…";
    case "continue":
      return "Resuming…";
    case "end_early":
      return "Ending early…";
    default:
      return "Applying…";
  }
}

export default function InterventionPanel({
  awaiting,
  onIntervene,
  activeSeats = [],
  operatorNote: operatorNoteProp,
  onOperatorNoteChange,
}: {
  awaiting: AwaitingInputData;
  onIntervene: (p: InterventionPayload) => void;
  activeSeats?: SeatRef[];
  /** Lifted note survives pause remounts when provided by DeliberateWorkspace. */
  operatorNote?: string;
  onOperatorNoteChange?: (note: string) => void;
}) {
  const [localNote, setLocalNote] = useState("");
  const operatorNote = operatorNoteProp ?? localNote;
  const setOperatorNote = onOperatorNoteChange ?? setLocalNote;
  const [showInject, setShowInject] = useState(true);
  const [showSwap, setShowSwap] = useState(false);
  const [swapSeat, setSwapSeat] = useState("");
  const [swapProvider, setSwapProvider] = useState("");
  const [swapModel, setSwapModel] = useState("");
  const [swapSystem, setSwapSystem] = useState("");
  const [prediction, setPrediction] = useState<InterventionPrediction | null>(null);
  const [pending, setPending] = useState<InterventionPayload["action"] | null>(null);

  useEffect(() => {
    let cancelled = false;
    setPrediction(null);
    api
      .predictIntervention(awaiting.convergence, awaiting.round_num)
      .then((p) => {
        if (!cancelled) setPrediction(p);
      })
      .catch(() => {
        if (!cancelled) setPrediction(null);
      });
    return () => {
      cancelled = true;
    };
  }, [awaiting.convergence, awaiting.round_num]);

  // Clear pending when a new pause arrives or the round advances.
  useEffect(() => {
    setPending(null);
  }, [awaiting.round_num, awaiting.convergence, awaiting.specops_signal]);

  const hint = predictHint(prediction);
  const showHint =
    hint.show && awaiting.options.includes("escalate_specops");

  const withNote = (p: InterventionPayload): InterventionPayload => {
    const note = operatorNote.trim();
    if (!note) return p;
    if (p.action === "inject_context") return { ...p, text: note };
    if (p.action.startsWith("escalate_")) return { ...p, text: note };
    return p;
  };

  const fire = (p: InterventionPayload) => {
    if (pending) return;
    setPending(p.action);
    onIntervene(withNote(p));
    if (p.action === "inject_context") {
      setOperatorNote("");
      setShowInject(false);
    }
  };

  const tone =
    awaiting.convergence >= 0.8
      ? "success"
      : awaiting.convergence >= 0.5
        ? "warning"
        : "danger";

  const convPct = Math.round(awaiting.convergence * 100);

  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      className={cn(
        "panel border-2 sticky top-20 z-20 overflow-hidden",
        tone === "success" && "border-success/40",
        tone === "warning" && "border-warning/40 animate-pulse-amber",
        tone === "danger" && "border-danger/40 animate-pulse-magenta",
      )}
    >
      <div className="flex items-center gap-2 px-4 py-3 border-b border-border bg-bg-deep/60">
        <Pause className={cn(
          "w-4 h-4",
          tone === "success" && "text-success",
          tone === "warning" && "text-warning",
          tone === "danger" && "text-danger",
        )} />
        <span className="font-display font-bold text-fg-bright">
          Awaiting your call
        </span>
      </div>

      <div className="p-4 space-y-3">
        <p className="text-[11px] font-mono text-fg-muted leading-relaxed">
          Round paused at{" "}
          <span className={cn(
            tone === "success" && "text-success",
            tone === "warning" && "text-warning",
            tone === "danger" && "text-danger",
          )}>
            {convPct}%
          </span>
          . Continue as-is, steer the next round with a note, or run an escalation lens
          before seats resume.
        </p>

        {pending && (
          <div className="flex items-center gap-2 px-2.5 py-2 border border-amber/35 bg-amber/[0.04] text-[11px] font-mono text-amber">
            <Loader2 className="w-3.5 h-3.5 animate-spin shrink-0" />
            {pendingLabel(pending)}
          </div>
        )}

        {awaiting.specops_signal && (
          <div className="border border-magenta/30 bg-magenta/5 rounded-md p-3 text-xs max-h-48 overflow-y-auto">
            <div className="flex items-center gap-1.5 mb-1.5">
              <Flame className="w-3 h-3 text-magenta shrink-0" />
              <span className="font-mono font-semibold text-magenta text-[10px] uppercase tracking-widest">
                SpecOps result
              </span>
            </div>
            <p className="text-[10px] font-mono text-fg-dim mb-2 leading-relaxed">
              Prior SpecOps escalation — review before continuing. Fed into the next round
              when you resume.
            </p>
            <div className="text-fg-muted leading-relaxed">{awaiting.specops_signal}</div>
          </div>
        )}

        <div className="grid grid-cols-2 gap-2">
          <ActionBtn
            icon={<Play className="w-3.5 h-3.5" />}
            label="Continue"
            tone="cyan"
            disabled={!!pending}
            onClick={() => fire({ action: "continue" })}
          />
          <ActionBtn
            icon={<StopCircle className="w-3.5 h-3.5" />}
            label="End early"
            tone="amber"
            disabled={!!pending}
            onClick={() => fire({ action: "end_early" })}
          />
        </div>

        {showHint && (
          <div
            data-testid="predict-hint"
            className="flex items-start gap-2 border border-magenta/30 bg-magenta/5 rounded-md p-2.5 text-xs text-fg-muted"
          >
            <Lightbulb className="w-3.5 h-3.5 text-magenta mt-0.5 shrink-0" />
            <span className="leading-snug">{hint.label}</span>
          </div>
        )}

        {awaiting.options.includes("escalate_specops") && (
        <div className="border-t border-border pt-3">
          <div className="label mb-2">Escalations</div>
          <div className="grid grid-cols-2 gap-2">
            <ActionBtn
              icon={<Crosshair className="w-3.5 h-3.5" />}
              label="SpecOps"
              sub={ESCALATION_HINTS.escalate_specops}
              tone="magenta"
              pending={pending === "escalate_specops"}
              disabled={!!pending}
              onClick={() => fire({ action: "escalate_specops" })}
            />
            <ActionBtn
              icon={<Brain className="w-3.5 h-3.5" />}
              label="Munger"
              sub={ESCALATION_HINTS.escalate_munger}
              tone="amber"
              pending={pending === "escalate_munger"}
              disabled={!!pending}
              onClick={() => fire({ action: "escalate_munger" })}
            />
            <ActionBtn
              icon={<Flame className="w-3.5 h-3.5" />}
              label="Contrarian"
              sub={ESCALATION_HINTS.escalate_contrarian}
              tone="magenta"
              pending={pending === "escalate_contrarian"}
              disabled={!!pending}
              onClick={() => fire({ action: "escalate_contrarian" })}
            />
            <ActionBtn
              icon={<Scissors className="w-3.5 h-3.5" />}
              label="KISS"
              sub={ESCALATION_HINTS.escalate_kiss}
              tone="cyan"
              pending={pending === "escalate_kiss"}
              disabled={!!pending}
              onClick={() => fire({ action: "escalate_kiss" })}
            />
          </div>
          {operatorNote.trim() && (
            <p className="mt-2 text-[10px] font-mono text-fg-dim leading-relaxed">
              Operator note will be included with the next escalation or inject.
            </p>
          )}
        </div>
        )}

        {awaiting.options.includes("swap_seat") && activeSeats.length > 0 && (
          <div className="border-t border-border pt-3">
            <button
              type="button"
              disabled={!!pending}
              onClick={() => setShowSwap((v) => !v)}
              className="flex items-center gap-2 text-xs text-fg-muted hover:text-fg disabled:opacity-40"
            >
              <Shuffle className="w-3.5 h-3.5" />
              {showSwap ? "Hide seat swap" : "Swap a seat for next round"}
            </button>
            {showSwap && (
              <div className="mt-2 space-y-2">
                <select
                  value={swapSeat}
                  onChange={(e) => setSwapSeat(e.target.value)}
                  className="input text-xs w-full"
                >
                  <option value="">Select seat…</option>
                  {activeSeats.map((s) => (
                    <option key={s.name} value={s.name}>
                      {s.name} ({s.provider}/{s.model})
                    </option>
                  ))}
                </select>
                <div className="grid grid-cols-2 gap-2">
                  <input
                    value={swapProvider}
                    onChange={(e) => setSwapProvider(e.target.value)}
                    placeholder="provider"
                    className="input text-xs"
                  />
                  <input
                    value={swapModel}
                    onChange={(e) => setSwapModel(e.target.value)}
                    placeholder="model"
                    className="input text-xs"
                  />
                </div>
                <textarea
                  value={swapSystem}
                  onChange={(e) => setSwapSystem(e.target.value)}
                  rows={2}
                  placeholder="system prompt override (leave empty to keep current)"
                  className="input text-xs font-mono"
                />
                <button
                  type="button"
                  onClick={() => {
                    if (!swapSeat) return;
                    fire({
                      action: "swap_seat",
                      seat_name: swapSeat,
                      ...(swapProvider && { provider: swapProvider }),
                      ...(swapModel && { model: swapModel }),
                      ...(swapSystem && { system: swapSystem }),
                    });
                    setShowSwap(false);
                    setSwapSeat("");
                    setSwapProvider("");
                    setSwapModel("");
                    setSwapSystem("");
                  }}
                  disabled={!swapSeat || !!pending}
                  className="btn btn-primary w-full text-xs"
                >
                  <Shuffle className="w-3.5 h-3.5" />
                  Swap &amp; resume
                </button>
              </div>
            )}
          </div>
        )}

        <div className="border-t border-border pt-3">
          <button
            type="button"
            disabled={!!pending}
            onClick={() => setShowInject((v) => !v)}
            className="flex items-center gap-2 text-xs text-fg-muted hover:text-fg disabled:opacity-40"
          >
            <MessagesSquare className="w-3.5 h-3.5" />
            {showInject ? "Hide operator note" : "Operator note for next round"}
          </button>
          {showInject && (
            <div className="mt-2 space-y-2">
              <textarea
                value={operatorNote}
                onChange={(e) => setOperatorNote(e.target.value)}
                rows={4}
                placeholder="Operator note — fed verbatim into every seat next round…"
                className="input text-xs"
                disabled={!!pending}
              />
              <button
                type="button"
                onClick={() => fire({ action: "inject_context", text: operatorNote.trim() })}
                disabled={!operatorNote.trim() || !!pending}
                className="btn btn-primary w-full text-xs"
              >
                {pending === "inject_context" ? (
                  <Loader2 className="w-3.5 h-3.5 animate-spin" />
                ) : (
                  <FastForward className="w-3.5 h-3.5" />
                )}
                Inject &amp; resume
              </button>
            </div>
          )}
        </div>
      </div>
    </motion.div>
  );
}

function ActionBtn({
  icon, label, sub, tone, onClick, disabled, pending,
}: {
  icon: React.ReactNode;
  label: string;
  sub?: string;
  tone: "amber" | "cyan" | "magenta";
  onClick: () => void;
  disabled?: boolean;
  pending?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "btn text-xs justify-start flex-col items-start gap-0.5 h-auto py-2",
        tone === "amber" && "btn-primary",
        tone === "cyan" && "btn-cyan",
        tone === "magenta" && "btn-magenta",
        disabled && "opacity-40 cursor-not-allowed",
      )}
    >
      <span className="flex items-center gap-1.5 w-full">
        {pending ? <Loader2 className="w-3.5 h-3.5 animate-spin shrink-0" /> : icon}
        <span>{label}</span>
      </span>
      {sub && (
        <span className="text-[9px] font-normal text-fg-dim pl-5 leading-tight">
          {sub}
        </span>
      )}
    </button>
  );
}
