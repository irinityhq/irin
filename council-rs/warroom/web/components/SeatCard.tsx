"use client";

import { motion } from "framer-motion";
import { AlertCircle, Loader2, Sparkles } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn, fmtCost, fmtLatency, fmtTokens, providerColor } from "@/lib/cn";
import type { SeatRuntimeState } from "@/lib/types";

export default function SeatCard({ seat }: { seat: SeatRuntimeState }) {
  const tone = providerColor(seat.seat.provider);
  const isThinking = seat.status === "thinking";
  const isComplete = seat.status === "complete";
  const isError = seat.status === "error";
  const usageUnavailable =
    seat.provider_provenance?.accounting === "usage_unavailable";
  const runner = seat.provider_provenance?.runner?.replace(/_/g, " ");

  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.35 }}
      className={cn(
        "relative rounded-md border bg-bg-overlay/60 overflow-hidden",
        isThinking
          ? toneBorder(tone, "thinking")
          : isComplete
            ? toneBorder(tone, "complete")
            : isError
              ? "border-danger/50"
              : "border-border",
        isThinking && pulseClass(tone),
      )}
    >
      <div className="flex items-center justify-between px-3 py-2 border-b border-border bg-bg-deep/40">
        <div className="flex items-center gap-2">
          <Sparkles className={cn("w-3.5 h-3.5", `text-${tone}`)} />
          <span className="font-display font-semibold text-fg-bright text-sm">
            {seat.seat.name}
          </span>
          <span className={cn("chip", `chip-${tone}`)}>
            {seat.seat.provider}
          </span>
          {runner && runner !== seat.seat.provider && (
            <span className="text-[10px] font-mono text-fg-dim">{runner}</span>
          )}
        </div>
        <div className="text-[10px] font-mono text-fg-dim">
          {seat.seat.model || ""}
        </div>
      </div>

      <div className="p-3 min-h-[140px] max-h-[400px] overflow-y-auto relative">
        {isThinking && !seat.text && (
          <div className="flex items-center gap-2 text-fg-muted">
            <Loader2 className="w-4 h-4 animate-spin" />
            <span className="font-mono text-xs uppercase tracking-widest">
              Reasoning…
            </span>
          </div>
        )}
        {isError && (
          <div className="flex items-start gap-2 text-danger">
            <AlertCircle className="w-4 h-4 mt-0.5 shrink-0" />
            <span className="font-mono text-xs">{seat.error}</span>
          </div>
        )}
        {seat.text && (
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            className="ruling text-sm max-w-none"
          >
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {seat.text}
            </ReactMarkdown>
            {seat.streaming && (
              <span
                data-testid="seat-stream-cursor"
                aria-hidden
                className="inline-block w-1.5 h-3.5 ml-0.5 align-text-bottom bg-cyan animate-pulse-cyan"
              />
            )}
          </motion.div>
        )}
      </div>

      {isComplete && (
        <div className="grid grid-cols-3 px-3 py-1.5 border-t border-border text-[10px] font-mono text-fg-dim">
          <span>{fmtLatency(seat.latency_ms)}</span>
          {usageUnavailable ? (
            <>
              <span>usage n/a</span>
              <span className="text-right">cost n/a</span>
            </>
          ) : (
            <>
              <span>
                {fmtTokens(seat.tokens_in + seat.tokens_out)} tok
                {seat.cached_in ? ` · ${fmtTokens(seat.cached_in)} cache` : ""}
              </span>
              <span className="text-right">{fmtCost(seat.cost_usd)}</span>
            </>
          )}
        </div>
      )}

      {isComplete && seat.gateway_provenance && (
        <div className="px-3 py-1.5 border-t border-border text-[10px] font-mono text-cyan truncate">
          gateway: {seat.gateway_provenance.gateway_request_id}
        </div>
      )}
    </motion.div>
  );
}

function toneBorder(tone: string, mode: "thinking" | "complete"): string {
  if (mode === "thinking") {
    return cn(`border-${tone}/50`);
  }
  return cn(`border-${tone}/30`);
}

function pulseClass(tone: string): string {
  switch (tone) {
    case "amber":
      return "animate-pulse-amber";
    case "cyan":
      return "animate-pulse-cyan";
    case "magenta":
      return "animate-pulse-magenta";
    default:
      return "animate-pulse-cyan";
  }
}
