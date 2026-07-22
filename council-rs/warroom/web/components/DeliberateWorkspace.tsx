"use client";

import { useEffect, useMemo, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Copy,
  Info,
  Loader2,
  RotateCcw,
  RotateCw,
  Scale,
  Sparkles,
  Zap,
} from "lucide-react";
import { cn } from "@/lib/cn";
import { useElapsedSeconds } from "@/hooks/useElapsedSeconds";
import { buildPhasesForLive } from "@/lib/proceeding-phases";
import type {
  BudgetPausedData,
  DeliberationState,
  InterventionPayload,
} from "@/lib/types";
import type { Cabinet, HealthResponse } from "@/lib/types";
import type { StartPayload } from "@/lib/ws";
import InterventionPanel from "./InterventionPanel";
import IdlePanel from "./IdlePanel";
import { ProceedingMetrics } from "./proceeding/ProceedingMetrics";
import { ProceedingPhaseRail } from "./proceeding/ProceedingPhaseRail";
import { ProceedingRecordHead } from "./proceeding/ProceedingRecordHead";
import { ProceedingRulingColumn } from "./proceeding/ProceedingRulingColumn";
import { RulingActions } from "./proceeding/RulingActions";
import { LiveRoundLedger } from "./proceeding/SeatLedger";

function liveMode(state: DeliberationState): string {
  if (state.mode === "blind") return "blind";
  return state.deliberation_mode ?? "teardown";
}

export default function DeliberateWorkspace({
  state,
  cabinets,
  health,
  onStart,
  onIntervene,
  onReset,
  onReconnect,
  canReconnect,
  onViewDriftReport,
  onViewOutbox,
  onViewHistory,
  initialCabinet,
  onConsumeInitialCabinet,
}: {
  state: DeliberationState;
  cabinets: Cabinet[];
  health: HealthResponse | null;
  onStart: (p: StartPayload) => void;
  onIntervene: (p: InterventionPayload) => void;
  onReset: () => void;
  onReconnect?: () => void;
  canReconnect?: boolean;
  onViewDriftReport?: (reportFilename: string) => void;
  onViewOutbox?: (tenant: string) => void;
  onViewHistory?: (sessionId?: string) => void;
  initialCabinet?: string | null;
  onConsumeInitialCabinet?: () => void;
}) {
  if (state.phase === "idle") {
    return (
      <IdlePanel
        variant="shell"
        cabinets={cabinets}
        onStart={onStart}
        health={health}
        onViewDriftReport={onViewDriftReport}
        initialCabinet={initialCabinet}
        onConsumeInitialCabinet={onConsumeInitialCabinet}
      />
    );
  }

  if (state.phase === "error") {
    return <DeliberateErrorShell state={state} onReset={onReset} onReconnect={onReconnect} canReconnect={canReconnect} />;
  }

  return (
    <DeliberateLiveShell
      state={state}
      onIntervene={onIntervene}
      onReset={onReset}
      onViewOutbox={onViewOutbox}
      onViewHistory={onViewHistory}
    />
  );
}

function DeliberateLiveShell({
  state,
  onIntervene,
  onReset,
  onViewOutbox,
  onViewHistory,
}: {
  state: DeliberationState;
  onIntervene: (p: InterventionPayload) => void;
  onReset: () => void;
  onViewOutbox?: (tenant: string) => void;
  onViewHistory?: (sessionId?: string) => void;
}) {
  const phases = useMemo(() => buildPhasesForLive(state), [state]);
  const current = state.rounds.find((r) => r.round_num === state.current_round);
  const prior = state.rounds.filter((r) => r.round_num < state.current_round);
  const finalConv =
    state.rounds.length > 0
      ? state.rounds[state.rounds.length - 1].convergence ?? 0
      : undefined;
  const isDone = state.phase === "done";
  const isSynthesizing = state.phase === "synthesizing";
  const isConnecting = state.phase === "connecting";
  const waitingForFirstRound =
    !isDone && !isSynthesizing && !current && state.rounds.length === 0;
  const elapsedSeconds = useElapsedSeconds(!isDone);

  const [operatorNote, setOperatorNote] = useState("");
  useEffect(() => {
    setOperatorNote("");
  }, [state.session_id]);

  const rulingHeaderActions =
    isDone && state.synthesis ? (
      <RulingInlineActions state={state} onReset={onReset} />
    ) : undefined;

  return (
    <div className="cg-history-workspace" data-testid="deliberate-workspace">
      <aside className="cg-rail cg-deliberate-rail">
        <DeliberateRail
          state={state}
          onIntervene={onIntervene}
          operatorNote={operatorNote}
          onOperatorNoteChange={setOperatorNote}
        />
      </aside>

      <div className="cg-record-primary">
        {(state.topic || state.session_id) && (
          <ProceedingRecordHead
            mode={liveMode(state)}
            cabinetLabel={state.cabinet_label || state.cabinet_name}
            topic={state.topic}
            sessionId={state.session_id || undefined}
            executionRoute={state.execution_route}
            gatewaySensitivity={state.gateway_sensitivity}
          />
        )}

        {!isConnecting && (
          <ProceedingMetrics
            rounds={state.rounds.length || state.current_round}
            tokens={state.totals.tokens}
            costUsd={state.totals.cost_usd}
            latencyMs={state.totals.latency_ms}
            convergence={finalConv}
          />
        )}

        {!isConnecting && <ProceedingPhaseRail phases={phases} />}

        {(isConnecting || waitingForFirstRound) && (
          <StartupHeartbeat
            elapsedSeconds={elapsedSeconds}
            title={
              isConnecting
                ? "Opening council channel"
                : "Council accepted the proceeding"
            }
            detail={
              isConnecting
                ? "WebSocket upgrade pending"
                : "Waiting on first round event"
            }
          />
        )}

        {!isConnecting && !waitingForFirstRound && state.phase === "specops" && !state.specops && (
          <MainColumnLoader label="Running SpecOps…" />
        )}
        {!isConnecting &&
          !waitingForFirstRound &&
          state.pendingIntervention === "end_early" &&
          state.phase !== "specops" &&
          !isSynthesizing &&
          !isDone && <MainColumnLoader label="Ending early…" />}

        {state.budget_paused && <BudgetBanner data={state.budget_paused} />}
        {state.specops && <SpecopsBanner specops={state.specops} />}
        {state.phase_label && state.phase !== "done" && !isConnecting && (
          <PhaseNote label={state.phase_label} />
        )}

        {!isConnecting &&
          prior.map((r) => (
            <LiveRoundLedger
              key={r.round_num}
              round={r}
              seats={state.active_seats}
              // Dimming is a live-only signal; the filed record reads flat like History.
              dimmed={!isDone}
            />
          ))}
        {!isConnecting && current && (
          <LiveRoundLedger
            round={current}
            seats={state.active_seats}
            live={state.phase === "streaming" || state.phase === "specops"}
          />
        )}

        {state.info_messages.length > 0 && (
          <OperatorLog messages={state.info_messages} />
        )}
      </div>

      <ProceedingRulingColumn
        synthesis={state.synthesis?.text}
        synthesisModel={state.synthesis?.model}
        sessionId={state.session_id || undefined}
        loading={isSynthesizing}
        loadingLabel={`Chair composing (${state.chair.provider} · ${state.chair.model})`}
        placeholder={
          isConnecting
            ? "Ruling will appear here when the chair files it."
            : "Awaiting chair synthesis."
        }
        headerActions={rulingHeaderActions}
        footer={
          isDone && state.synthesis ? (
            <RulingActions
              state={state}
              onReset={onReset}
              onViewOutbox={onViewOutbox}
              onViewHistory={onViewHistory}
            />
          ) : undefined
        }
      />
    </div>
  );
}

function MainColumnLoader({ label }: { label: string }) {
  return (
    <div className="border-b border-border bg-bg-panel/70 px-4 py-4">
      <div className="flex items-center gap-3">
        <Loader2 className="h-4 w-4 shrink-0 animate-spin text-amber" />
        <span className="font-mono text-xs text-fg-muted">{label}</span>
      </div>
    </div>
  );
}

function StartupHeartbeat({
  elapsedSeconds,
  title,
  detail,
}: {
  elapsedSeconds: number;
  title: string;
  detail: string;
}) {
  return (
    <div className="border-b border-border bg-bg-panel/70 px-4 py-5">
      <div className="flex items-center justify-between gap-4">
        <div className="flex min-w-0 items-center gap-3">
          <Loader2 className="h-4 w-4 shrink-0 animate-spin text-amber" />
          <div className="min-w-0">
            <div className="font-authority text-sm font-semibold text-fg-bright">
              {title}
            </div>
            <div className="font-mono text-xs text-fg-muted">{detail}</div>
          </div>
        </div>
        <div className="shrink-0 font-mono text-xs tabular-nums text-amber">
          {elapsedSeconds}s
        </div>
      </div>
    </div>
  );
}

function DeliberateErrorShell({
  state,
  onReset,
  onReconnect,
  canReconnect,
}: {
  state: DeliberationState;
  onReset: () => void;
  onReconnect?: () => void;
  canReconnect?: boolean;
}) {
  const lastError = state.errors[state.errors.length - 1];
  const isConnectionLoss = lastError?.message === "Connection to council bridge lost";

  return (
    <div className="cg-history-workspace">
      <aside className="cg-rail cg-deliberate-rail">
        <div className="p-3 text-[11px] font-mono text-fg-dim">Proceeding halted</div>
      </aside>
      <div className="cg-record-primary">
        {state.topic && (
          <ProceedingRecordHead
            mode={liveMode(state)}
            cabinetLabel={state.cabinet_label}
            topic={state.topic}
            sessionId={state.session_id || undefined}
            executionRoute={state.execution_route}
            gatewaySensitivity={state.gateway_sensitivity}
          />
        )}
        <div className="p-6 border-b border-danger/30 bg-danger/5">
          <div className="text-danger font-authority font-semibold text-lg mb-2">
            {isConnectionLoss ? "Connection lost" : "Deliberation aborted"}
          </div>
          {state.errors.map((e, i) => (
            <div key={i} className="font-mono text-sm text-fg-muted">
              {e.message}
            </div>
          ))}
          <div className="flex items-center gap-3 mt-6">
            {canReconnect && onReconnect && (
              <button type="button" onClick={onReconnect} className="btn btn-primary">
                <RotateCw className="w-4 h-4" />
                Reconnect
              </button>
            )}
            <button
              type="button"
              onClick={onReset}
              className={cn("btn", canReconnect ? "btn-danger" : "btn-primary")}
            >
              Reset
            </button>
          </div>
        </div>
      </div>
      <ProceedingRulingColumn placeholder="No ruling filed." />
    </div>
  );
}

function DeliberateRail({
  state,
  onIntervene,
  operatorNote,
  onOperatorNoteChange,
}: {
  state: DeliberationState;
  onIntervene: (p: InterventionPayload) => void;
  operatorNote: string;
  onOperatorNoteChange: (note: string) => void;
}) {
  // Filed record: live operator tools are gone; collapse to a quiet context teaser.
  if (state.phase === "done") {
    return <DoneContextRail state={state} />;
  }
  return (
    <>
      {state.phase === "paused" && state.awaiting && (
        <div className="cg-intervention-rail">
          <p className="cg-section-label mb-2">Operator intervention</p>
          <InterventionPanel
            awaiting={state.awaiting}
            onIntervene={onIntervene}
            activeSeats={state.active_seats}
            operatorNote={operatorNote}
            onOperatorNoteChange={onOperatorNoteChange}
          />
        </div>
      )}
      {state.precedent.length > 0 && (
        <PrecedentRail matches={state.precedent} />
      )}
      <p className="cg-section-label">Seat roster</p>
      <SeatRoster state={state} />
    </>
  );
}

/** Done-state rail — one-line teasers with disclosures, same pattern as the idle rail summary. */
function DoneContextRail({ state }: { state: DeliberationState }) {
  const [rosterOpen, setRosterOpen] = useState(false);
  const [precedentOpen, setPrecedentOpen] = useState(false);
  const cabinetLabel = state.cabinet_label || state.cabinet_name;

  return (
    <>
      <p className="cg-section-label">Proceeding context</p>
      <div className="cg-command-panel cg-command-panel--tight">
        <button
          type="button"
          onClick={() => setRosterOpen((v) => !v)}
          aria-expanded={rosterOpen}
          data-testid="done-rail-roster"
          className="w-full flex items-center gap-1.5 text-left text-[10px] font-mono leading-tight hover:text-fg transition-colors"
        >
          <span className="text-fg-dim shrink-0">{rosterOpen ? "▾" : "▸"}</span>
          <span className="text-amber font-semibold shrink-0">{cabinetLabel}</span>
          <span className="text-fg-dim truncate">
            · {state.active_seats.length} seats · chair {state.chair.provider}
          </span>
        </button>
        {rosterOpen && (
          <div className="mt-2 pt-2 border-t border-border">
            <SeatRoster state={state} bare />
          </div>
        )}
      </div>
      {state.precedent.length > 0 && (
        <div className="cg-command-panel cg-command-panel--tight mt-2">
          <button
            type="button"
            onClick={() => setPrecedentOpen((v) => !v)}
            aria-expanded={precedentOpen}
            className="w-full flex items-center gap-1.5 text-left text-[10px] font-mono leading-tight hover:text-fg transition-colors"
          >
            <span className="text-fg-dim shrink-0">{precedentOpen ? "▾" : "▸"}</span>
            <span className="text-fg-muted">
              {state.precedent.length} precedent match
              {state.precedent.length === 1 ? "" : "es"} at filing
            </span>
          </button>
          {precedentOpen && (
            <div className="mt-2 pt-2 border-t border-border space-y-2">
              {state.precedent.slice(0, 5).map((m) => (
                <div
                  key={m.id}
                  className="text-[10px] font-mono border-l-2 border-amber/40 pl-2 py-0.5"
                >
                  <div className="text-amber/80 flex items-center gap-1">
                    <span className="truncate">{m.id}</span>
                    {m.score != null && (
                      <span
                        className="ml-auto text-cyan tabular-nums"
                        title={m.why}
                      >
                        {Math.round(m.score * 100)}%
                      </span>
                    )}
                  </div>
                  <div className="text-fg-muted line-clamp-2">{m.topic}</div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </>
  );
}

function PrecedentRail({
  matches,
}: {
  matches: DeliberationState["precedent"];
}) {
  return (
    <div className="cg-command-panel mb-3">
      <div className="flex items-center gap-2 mb-2">
        <Sparkles className="w-3.5 h-3.5 text-amber" />
        <span className="cg-section-label mb-0">Precedent loaded</span>
      </div>
      <div className="space-y-2 max-h-40 overflow-y-auto">
        {matches.slice(0, 5).map((m) => (
          <div
            key={m.id}
            className="text-[10px] font-mono border-l-2 border-amber/40 pl-2 py-0.5"
          >
            <div className="text-amber/80 flex items-center gap-1">
              <span className="truncate">{m.id}</span>
              {m.score != null && (
                <span className="ml-auto text-cyan tabular-nums" title={m.why}>
                  {Math.round(m.score * 100)}%
                </span>
              )}
            </div>
            <div className="text-fg-muted line-clamp-2">{m.topic}</div>
            {m.why && <div className="text-fg-dim truncate">{m.why}</div>}
          </div>
        ))}
      </div>
    </div>
  );
}

function SeatRoster({ state, bare }: { state: DeliberationState; bare?: boolean }) {
  return (
    <div className={bare ? undefined : "cg-command-panel"}>
      <div className="space-y-1.5">
        {state.active_seats.map((s) => (
          <div
            key={s.name}
            className="flex items-center justify-between text-[10px] font-mono"
          >
            <span className="text-fg">{s.name}</span>
            <span className="text-fg-dim">{s.provider}</span>
          </div>
        ))}
        {state.dropped_seats.length > 0 && (
          <>
            <div className="border-t border-border pt-2 mt-2 label text-danger text-[10px]">
              Missing
            </div>
            {state.dropped_seats.map((s) => (
              <div
                key={s.name}
                className="flex items-center justify-between text-[10px] font-mono opacity-50"
              >
                <span className="line-through">{s.name}</span>
                <span>{s.provider}</span>
              </div>
            ))}
          </>
        )}
        <div className="flex items-center justify-between text-[10px] font-mono pt-2 border-t border-border">
          <span className="text-amber">Chair</span>
          <span className="text-fg-dim">{state.chair.provider}</span>
        </div>
      </div>
    </div>
  );
}

function BudgetBanner({ data }: { data: BudgetPausedData }) {
  return (
    <div className="mx-3.5 my-2 px-3 py-2 border border-amber/35 bg-amber/[0.04] text-[11px] font-mono text-fg-muted">
      <Scale className="w-3.5 h-3.5 text-amber inline mr-1.5 -mt-0.5" />
      <strong className="text-amber">Budget pause</strong>
      {" — "}${data.total_cost_usd.toFixed(4)} reached cap ${data.max_usd.toFixed(2)} after
      round {data.round_num}; ending early.
    </div>
  );
}

function SpecopsBanner({
  specops,
}: {
  specops: NonNullable<DeliberationState["specops"]>;
}) {
  return (
    <div className="mx-3.5 my-2 px-3 py-2 border border-magenta/35 bg-magenta/[0.04] text-[11px]">
      <div className="flex items-center gap-2 mb-1">
        <Zap className="w-3.5 h-3.5 text-magenta" />
        <span className="label text-magenta text-[10px]">SpecOps signal</span>
        <span className="chip chip-magenta text-[10px]">{specops.model}</span>
      </div>
      <div className="text-fg-muted leading-relaxed">{specops.text}</div>
    </div>
  );
}

function PhaseNote({ label }: { label: string }) {
  return (
    <div className="px-3.5 py-2 border-b border-border text-[11px] font-mono text-fg-dim bg-bg-elevated/50">
      {label}
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
    <div className="border-t border-border">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="w-full flex items-center gap-2 px-3.5 py-2 text-[11px] font-mono hover:bg-bg-overlay transition-colors"
      >
        <Info className="w-3.5 h-3.5 text-fg-dim shrink-0" />
        <span className="label text-fg-dim text-[10px]">Operator log</span>
        <span className="chip text-[10px]">{messages.length}</span>
        {!expanded && latest && (
          <span className="text-fg-dim truncate ml-1 flex-1 text-left">{latest.message}</span>
        )}
        <span className="text-fg-dim ml-auto shrink-0">
          {expanded ? <ChevronDown className="w-3 h-3" /> : <ChevronRight className="w-3 h-3" />}
        </span>
      </button>
      {expanded && (
        <div className="px-3.5 pb-3 space-y-1.5 max-h-40 overflow-y-auto border-t border-border">
          {messages.map((m, i) => (
            <div
              key={`${m.ts}-${i}`}
              className="text-[10px] font-mono border-l-2 border-border pl-2 py-0.5"
            >
              <div className="text-fg-dim">{m.ts}</div>
              <div className="text-fg-muted">{m.message}</div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Compact actions in ruling kicker when done — always above the scroll fold. */
function RulingInlineActions({
  state,
  onReset,
}: {
  state: DeliberationState;
  onReset: () => void;
}) {
  const [copied, setCopied] = useState(false);
  if (!state.synthesis) return null;

  return (
    <div className="flex items-center gap-1.5">
      <button
        type="button"
        className="btn text-[9px] py-0.5"
        onClick={async () => {
          await navigator.clipboard.writeText(state.synthesis!.text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}
      >
        <Copy className="w-3 h-3" />
        {copied ? "Copied" : "Copy"}
      </button>
      <button
        type="button"
        className="btn btn-primary text-[9px] py-0.5"
        onClick={onReset}
        data-testid="new-deliberation-header"
      >
        <RotateCcw className="w-3 h-3" />
        New
      </button>
    </div>
  );
}
