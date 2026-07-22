"use client";

import { useState } from "react";
import { Copy, Download, FileDown, RotateCcw } from "lucide-react";
import { fmtCost, fmtLatency } from "@/lib/cn";
import type { DeliberationState } from "@/lib/types";
import { downloadSessionPdf } from "@/lib/pdf-export";
import { isTauri, saveSynthesisNative } from "@/lib/tauri";

function directiveOutboxTenant(synthesis: string): string {
  const m = synthesis.match(/"tenant"\s*:\s*"([^"]+)"/);
  return m?.[1] ?? "system";
}

export function RulingActions({
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

  const canExportPdf = Boolean(state.saved_path && state.session_id);
  const directiveProposal = state.synthesis.text.includes(
    "irin.directive.proposal.v1",
  );

  const copy = async () => {
    await navigator.clipboard.writeText(state.synthesis!.text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const downloadJson = () => {
    const blob = new Blob([JSON.stringify(state, null, 2)], {
      type: "application/json",
    });
    triggerDownload(blob, `council_${state.session_id}.json`);
  };

  const saveMd = async () => {
    const md = [
      `# Council Ruling — ${state.session_id}`,
      ``,
      `**Topic:** ${state.topic}`,
      `**Cabinet:** ${state.cabinet_label}`,
      `**Mode:** ${state.mode}`,
      `**Tokens:** ${state.totals.tokens.toLocaleString()} · **Cost:** ${fmtCost(state.totals.cost_usd)} · **Latency:** ${fmtLatency(state.totals.latency_ms)}`,
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

  const exportPdf = async () => {
    if (!state.session_id) return;
    setExporting(true);
    try {
      await downloadSessionPdf(state.session_id);
    } finally {
      setExporting(false);
    }
  };

  return (
    <div className="space-y-3 border border-border rounded bg-bg-elevated p-3">
      <div className="flex flex-wrap gap-1.5">
        <button type="button" onClick={() => void copy()} className="btn text-[10px]">
          <Copy className="w-3.5 h-3.5" /> {copied ? "Copied" : "Copy"}
        </button>
        <button type="button" onClick={() => void saveMd()} className="btn text-[10px]">
          <Download className="w-3.5 h-3.5" /> {isTauri() ? "Save" : ".md"}
        </button>
        <button type="button" onClick={downloadJson} className="btn text-[10px]">
          <Download className="w-3.5 h-3.5" /> .json
        </button>
        {canExportPdf && (
          <button
            type="button"
            onClick={() => void exportPdf()}
            disabled={exporting}
            data-testid="synthesis-export-pdf"
            className="btn text-[10px]"
          >
            <FileDown className="w-3.5 h-3.5" /> {exporting ? "…" : "PDF"}
          </button>
        )}
        {directiveProposal && onViewOutbox && (
          <button
            type="button"
            onClick={() => onViewOutbox(directiveOutboxTenant(state.synthesis!.text))}
            className="btn text-[10px] border-cyan/40 text-cyan"
          >
            directive → Outbox
          </button>
        )}
      </div>
      {state.saved_path && (
        <p className="text-[10px] font-mono text-fg-dim truncate" title={state.saved_path}>
          Saved → {state.saved_path}
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        <button
          type="button"
          onClick={() => onViewHistory?.(state.session_id)}
          className="btn btn-primary text-[10px] flex-1"
        >
          View in History
        </button>
        <button type="button" onClick={onReset} className="btn text-[10px]">
          <RotateCcw className="w-3.5 h-3.5" />
          New deliberation
        </button>
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
