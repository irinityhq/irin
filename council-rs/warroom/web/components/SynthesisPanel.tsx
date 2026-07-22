"use client";

import { useState } from "react";
import { motion } from "framer-motion";
import { Copy, Crown, Download, FileDown, RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn, fmtCost, fmtLatency, fmtTokens } from "@/lib/cn";
import type { DeliberationState } from "@/lib/types";
import { downloadSessionPdf } from "@/lib/pdf-export";
import { isTauri, saveSynthesisNative } from "@/lib/tauri";

function directiveOutboxTenant(synthesis: string): string {
  const m = synthesis.match(/"tenant"\s*:\s*"([^"]+)"/);
  return m?.[1] ?? "system";
}

export default function SynthesisPanel({
  state,
  onReset,
  onViewOutbox,
  onViewHistory,
}: {
  state: DeliberationState;
  onReset: () => void;
  onViewOutbox?: (tenant: string) => void;
  onViewHistory?: (sessionId?: string) => void;
}) {
  const [copied, setCopied] = useState(false);
  const [exporting, setExporting] = useState(false);

  if (!state.synthesis) return null;

  // N06 — the PDF endpoint reads the SAVED session, so the button is gated on
  // session persistence (saved_path / session_id present after session_saved).
  const canExportPdf = Boolean(state.saved_path && state.session_id);
  const exportPdf = async () => {
    if (!state.session_id) return;
    setExporting(true);
    try {
      await downloadSessionPdf(state.session_id);
    } catch {
      // best-effort; no toast wiring in this panel.
    } finally {
      setExporting(false);
    }
  };

  const directiveProposal =
    state.synthesis.text.includes("irin.directive.proposal.v1");

  const copy = async () => {
    await navigator.clipboard.writeText(state.synthesis!.text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const downloadJson = () => {
    const blob = new Blob([JSON.stringify(state, null, 2)],
                          { type: "application/json" });
    triggerDownload(blob, `council_${state.session_id}.json`);
  };

  const saveMd = async () => {
    const md = [
      `# Council Ruling — ${state.session_id}`,
      ``,
      `**Topic:** ${state.topic}`,
      `**Cabinet:** ${state.cabinet_label}`,
      `**Mode:** ${state.mode}`,
      `**Tokens:** ${state.totals.tokens.toLocaleString()} · `
        + `**Cost:** ${fmtCost(state.totals.cost_usd)} · `
        + `**Latency:** ${fmtLatency(state.totals.latency_ms)}`,
      ``,
      `---`,
      ``,
      state.synthesis!.text,
    ].join("\n");
    if (isTauri()) {
      try {
        await saveSynthesisNative(md);
      } catch {
        const blob = new Blob([md], { type: "text/markdown" });
        triggerDownload(blob, `council_${state.session_id}.md`);
      }
      return;
    }
    const blob = new Blob([md], { type: "text/markdown" });
    triggerDownload(blob, `council_${state.session_id}.md`);
  };

  return (
    <motion.div
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      className="space-y-6"
    >
      <div className="panel-glass border-amber/40 p-6 relative overflow-hidden">
        <div className="absolute inset-0 bg-amber-radial opacity-40 pointer-events-none" />
        <div className="relative">
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-2">
              <Crown className="w-5 h-5 text-amber" />
              <span className="font-display font-bold text-fg-bright text-lg">
                Council Ruling
              </span>
              <span className="chip chip-amber">{state.synthesis.model}</span>
              {directiveProposal && (
                onViewOutbox ? (
                  <button
                    type="button"
                    onClick={() => onViewOutbox(directiveOutboxTenant(state.synthesis!.text))}
                    className="chip chip-cyan hover:bg-cyan/20"
                    title="Open Gateway Outbox for directive tenant"
                  >
                    directive_proposal_v1 → Outbox
                  </button>
                ) : (
                  <span
                    className="chip chip-cyan"
                    title="Chair used directive_proposal_v1"
                  >
                    directive_proposal_v1
                  </span>
                )
              )}
            </div>
            <div className="flex items-center gap-2">
              <button onClick={copy} className="btn text-xs">
                <Copy className="w-3.5 h-3.5" /> {copied ? "Copied" : "Copy"}
              </button>
              <button onClick={() => void saveMd()} className="btn text-xs">
                <Download className="w-3.5 h-3.5" /> {isTauri() ? "Save" : ".md"}
              </button>
              <button onClick={downloadJson} className="btn text-xs">
                <Download className="w-3.5 h-3.5" /> .json
              </button>
              {canExportPdf && (
                <button
                  onClick={() => void exportPdf()}
                  disabled={exporting}
                  data-testid="synthesis-export-pdf"
                  title="Download this session as a PDF"
                  className="btn text-xs"
                >
                  <FileDown className="w-3.5 h-3.5" />{" "}
                  {exporting ? "…" : "PDF"}
                </button>
              )}
            </div>
          </div>
          <article className="ruling max-w-none">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {state.synthesis.text}
            </ReactMarkdown>
          </article>
        </div>
      </div>

      <div className="panel p-5 grid grid-cols-2 md:grid-cols-4 gap-4">
        <Stat label="Total tokens" value={fmtTokens(state.totals.tokens)} tone="cyan" />
        <Stat label="Total cost" value={fmtCost(state.totals.cost_usd)} tone="amber" />
        <Stat label="Total latency" value={fmtLatency(state.totals.latency_ms)} tone="muted" />
        <Stat
          label="Final convergence"
          value={
            state.rounds.length
              ? `${Math.round((state.rounds[state.rounds.length - 1].convergence ?? 0) * 100)}%`
              : "—"
          }
          tone="success"
        />
      </div>

      <div className="flex justify-between items-center">
        <div className="text-xs font-mono text-fg-dim">
          {state.saved_path && `Saved → ${state.saved_path}`}
        </div>
        <div className="flex gap-2">
          <button
            onClick={() => onViewHistory?.(state.session_id)}
            className="btn btn-primary"
          >
            View full session in History
          </button>
          <button onClick={onReset} className="btn">
            <RotateCcw className="w-4 h-4" />
            New deliberation
          </button>
        </div>
      </div>
    </motion.div>
  );
}

function Stat({
  label, value, tone,
}: {
  label: string; value: string; tone: "cyan" | "amber" | "muted" | "success";
}) {
  return (
    <div>
      <div className="label">{label}</div>
      <div
        className={cn(
          "font-display font-bold text-2xl tabular-nums mt-1",
          tone === "cyan" && "text-cyan",
          tone === "amber" && "text-amber",
          tone === "muted" && "text-fg",
          tone === "success" && "text-success",
        )}
      >
        {value}
      </div>
    </div>
  );
}

function triggerDownload(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}
