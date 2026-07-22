"use client";

import { useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useElapsedSeconds } from "@/hooks/useElapsedSeconds";
import { cn, convergenceTone, fmtCost, fmtTokens, providerLedgerClass } from "@/lib/cn";
import type {
  RoundRuntimeState,
  RoundValidationData,
  SeatRef,
  SeatRuntimeState,
  SessionResponse,
  SessionRound,
} from "@/lib/types";
import { ValidationStrip } from "./ValidationStrip";

export function seatPreview(text: string): string {
  const t = (text || "").replace(/\s+/g, " ").trim();
  if (t.length <= 160) return t || "—";
  return `${t.slice(0, 157)}…`;
}

export function HistoryRoundLedger({ round }: { round: SessionRound }) {
  const conv = round.convergence_score ?? 0;
  return (
    <section className="border-b border-border bg-bg-deep/50">
      <div className="flex items-center gap-2 px-3.5 py-2 border-b border-border bg-bg-elevated">
        <span className="label">Round {round.round_num}</span>
        <span
          className={cn(
            "chip ml-auto text-[10px]",
            convergenceTone(conv) === "success" && "chip-success",
            convergenceTone(conv) === "warning" && "chip-amber",
            convergenceTone(conv) === "danger" && "chip-danger",
          )}
        >
          {Math.round(conv * 100)}%
        </span>
        {round.converged && <span className="chip text-[10px]">CONVERGED</span>}
      </div>
      <div className="px-3.5 py-1">
        <p className="cg-section-label mb-2 mt-1">Seat ledger</p>
        {round.responses.map((seat, i) => (
          <LedgerSeatRow key={`${seat.seat_name}-${i}`} seat={seat} />
        ))}
      </div>
      {round.validation_report && round.validation_report.length > 0 && (
        <ValidationStrip
          variant="ledger"
          validation={{
            round_num: round.round_num,
            gate_applied: round.responses.some((r) =>
              (r.text || "").includes("[REDACTED"),
            ),
            verdicts: round.validation_report as RoundValidationData["verdicts"],
          }}
        />
      )}
    </section>
  );
}

export function LedgerSeatRow({ seat }: { seat: SessionResponse }) {
  const [open, setOpen] = useState(false);
  const preview = seatPreview(seat.text);
  return (
    <div className={cn("cg-seat-row", providerLedgerClass(seat.provider))}>
      <div className="text-[10px] font-mono text-fg-muted min-w-0">
        <b className="block text-fg text-[11px] font-semibold">{seat.seat_name}</b>
        {seat.provider}
      </div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="text-left text-fg-muted leading-snug hover:text-fg transition-colors"
      >
        {seat.error ? (
          <span className="text-danger font-mono text-[11px]">{seat.error}</span>
        ) : open ? (
          <article className="ruling max-w-none">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{seat.text}</ReactMarkdown>
          </article>
        ) : (
          <span className="text-[12px]">{preview}</span>
        )}
        {!seat.error && seat.text && seat.text.length > preview.length && (
          <span className="block text-[10px] font-mono text-fg-dim mt-1">
            {open ? "Collapse" : "Expand full response"}
          </span>
        )}
      </button>
      <div className="text-right text-[10px] font-mono text-fg-dim leading-relaxed">
        {fmtTokens(seat.tokens_in + seat.tokens_out)}
        <br />
        <span className="text-amber">{fmtCost(seat.cost_usd)}</span>
      </div>
    </div>
  );
}

export function LiveRoundLedger({
  round,
  seats,
  dimmed,
  live,
}: {
  round: RoundRuntimeState;
  seats: SeatRef[];
  dimmed?: boolean;
  live?: boolean;
}) {
  const conv = round.convergence ?? 0;
  const scoringPending =
    !!live &&
    round.convergence == null &&
    !round.complete &&
    seats.length > 0 &&
    seats.every((s) => {
      const st = round.seats[s.name]?.status;
      return st === "complete" || st === "error";
    });
  const scoringElapsed = useElapsedSeconds(scoringPending);
  return (
    <section
      className={cn(
        "border-b border-border bg-bg-deep/50",
        dimmed && "opacity-70",
        // Enter animation only while the round is in flight — never replays on
        // graduation to prior or when the record files (done → flat rounds).
        !dimmed && !round.complete && "cg-ledger-round-enter",
        live && "ring-1 ring-inset ring-amber/20",
      )}
    >
      <div className="flex items-center gap-2 px-3.5 py-2 border-b border-border bg-bg-elevated">
        <span className="label">Round {round.round_num}</span>
        {round.convergence != null && (
          <span
            className={cn(
              "chip ml-auto text-[10px]",
              convergenceTone(conv) === "success" && "chip-success",
              convergenceTone(conv) === "warning" && "chip-amber",
              convergenceTone(conv) === "danger" && "chip-danger",
            )}
          >
            {Math.round(conv * 100)}%
          </span>
        )}
        {/* Filed rounds adopt History's vocabulary; EARLY CONVERGENCE is a transient live signal. */}
        {round.complete && (round.converged || round.early_convergence) ? (
          <span className="chip text-[10px]">CONVERGED</span>
        ) : round.early_convergence ? (
          <span className="chip text-[10px]">EARLY CONVERGENCE</span>
        ) : null}
        {live && !round.complete && (
          <span className="text-[9px] font-mono text-fg-dim uppercase tracking-wider">
            in session
          </span>
        )}
      </div>
      <div className="px-3.5 py-1">
        <p className="cg-section-label mb-2 mt-1">Seat ledger</p>
        {seats.map((seatRef) => (
          <LiveLedgerSeatRow
            key={seatRef.name}
            seatRef={seatRef}
            seat={round.seats[seatRef.name]}
            reserved
          />
        ))}
      </div>
      {scoringPending && (
        <div className="px-3.5 pb-2.5 flex items-center gap-1.5 text-[11px] font-mono text-amber/90 italic">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-current animate-pulse" aria-hidden />
          Scoring convergence &amp; validating…
          <span className="ml-1 tabular-nums not-italic text-fg-dim">{scoringElapsed}s</span>
        </div>
      )}
      {round.validation && (
        <ValidationStrip variant="ledger" validation={round.validation} />
      )}
    </section>
  );
}

export function LiveLedgerSeatRow({
  seatRef,
  seat,
  reserved,
}: {
  seatRef: SeatRef;
  seat?: SeatRuntimeState;
  reserved?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const status = seat?.status ?? "pending";
  const preview = seat?.text ? seatPreview(seat.text) : null;
  const inFlight = status === "thinking" || seat?.streaming;
  const pending = status === "pending" && !seat?.text;
  const composingElapsed = useElapsedSeconds(!!inFlight);

  return (
    <div
      className={cn(
        "cg-seat-row",
        providerLedgerClass(seatRef.provider),
        reserved && (pending || inFlight) && "cg-seat-row--reserved",
        inFlight && "cg-seat-row--composing",
      )}
    >
      <div className="text-[10px] font-mono text-fg-muted min-w-0">
        <b className="block text-fg text-[11px] font-semibold">{seatRef.name}</b>
        {seatRef.provider}
      </div>
      <div className="text-left text-fg-muted leading-snug min-h-[2.5rem]">
        {seat?.error ? (
          <span className="text-danger font-mono text-[11px]">{seat.error}</span>
        ) : preview ? (
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="text-left hover:text-fg transition-colors w-full"
          >
            {open ? (
              <article className="ruling max-w-none">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{seat!.text}</ReactMarkdown>
              </article>
            ) : (
              <span className="text-[12px]">{preview}</span>
            )}
            {seat!.text.length > preview.length && (
              <span className="block text-[10px] font-mono text-fg-dim mt-1">
                {open ? "Collapse" : "Expand full response"}
              </span>
            )}
          </button>
        ) : inFlight ? (
          <span className="cg-seat-status text-[12px] flex items-center gap-1.5 text-amber/90 italic">
            <span className="inline-block w-1.5 h-1.5 rounded-full bg-current animate-pulse" aria-hidden />
            Composing…
            <span className="ml-1 tabular-nums not-italic text-fg-dim">{composingElapsed}s</span>
          </span>
        ) : (
          <span className="cg-seat-status text-[12px] text-fg-dim">—</span>
        )}
      </div>
      <div className="text-right text-[10px] font-mono text-fg-dim leading-relaxed">
        {seat && (seat.tokens_in > 0 || seat.tokens_out > 0) ? (
          <>
            {fmtTokens(seat.tokens_in + seat.tokens_out)}
            <br />
            <span className="text-amber">{fmtCost(seat.cost_usd)}</span>
          </>
        ) : (
          "—"
        )}
      </div>
    </div>
  );
}
