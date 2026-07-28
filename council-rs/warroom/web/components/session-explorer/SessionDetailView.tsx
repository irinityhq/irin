import { useMemo, useState } from "react";
import {
  ArrowLeftRight,
  FileDown,
  GitBranch,
  GitFork,
  Layers,
  ScrollText,
} from "lucide-react";
import { cn } from "@/lib/cn";
import { downloadSessionPdf } from "@/lib/pdf-export";
import { buildPhasesForSession } from "@/lib/proceeding-phases";
import type { LineageResponse, SessionDetail, SessionIndexEntry } from "@/lib/types";
import { ProceedingMetrics } from "../proceeding/ProceedingMetrics";
import { ProceedingRecordHead } from "../proceeding/ProceedingRecordHead";
import { ProceedingPhaseRail } from "../proceeding/ProceedingPhaseRail";
import { ProceedingRulingColumn } from "../proceeding/ProceedingRulingColumn";
import { HistoryRoundLedger } from "../proceeding/SeatLedger";

export function SessionDetailView({
  detail,
  entry,
  lineage,
  onFork,
  onToggleCompare,
  compareActive,
  canFork,
}: {
  detail: SessionDetail;
  entry: SessionIndexEntry;
  lineage: LineageResponse | null;
  onFork: () => void;
  onToggleCompare: () => void;
  compareActive: boolean;
  canFork: boolean;
}) {
  const [synthesisOnly, setSynthesisOnly] = useState(false);
  const [exporting, setExporting] = useState(false);
  const phases = useMemo(() => buildPhasesForSession(detail), [detail]);

  const finalConv = detail.rounds.length
    ? detail.rounds[detail.rounds.length - 1].convergence_score ?? 0
    : entry.convergence ?? 0;
  const exportPdf = async () => {
    setExporting(true);
    try {
      await downloadSessionPdf(detail.session_id);
    } finally {
      setExporting(false);
    }
  };

  return (
    <>
      <div className="cg-record-primary">
        <ProceedingRecordHead
          key={detail.session_id}
          mode={detail.mode}
          cabinetLabel={detail.cabinet_name}
          topic={detail.topic}
          sessionId={detail.session_id}
          executionRoute={detail.execution_route ?? entry.execution_route}
          gatewaySensitivity={detail.gateway_sensitivity ?? entry.gateway_sensitivity}
          actions={
            <>
              <button
                type="button"
                onClick={onFork}
                className="btn btn-primary text-[10px]"
                disabled={!canFork}
              >
                <GitFork className="w-3.5 h-3.5" />
                Fork
              </button>
              <button
                type="button"
                onClick={onToggleCompare}
                className={cn("btn text-[10px]", compareActive && "btn-primary")}
              >
                <ArrowLeftRight className="w-3.5 h-3.5" />
                {compareActive ? "Cancel diff" : "Diff"}
              </button>
              <button
                type="button"
                data-testid="session-export-pdf"
                onClick={() => void exportPdf()}
                disabled={exporting}
                className="btn text-[10px]"
              >
                <FileDown className="w-3.5 h-3.5" />
                {exporting ? "…" : "Export"}
              </button>
              <button
                type="button"
                data-testid="synthesis-only-toggle"
                onClick={() => setSynthesisOnly((v) => !v)}
                title={
                  synthesisOnly
                    ? "Show full round-by-round record"
                    : "Focus on Council ruling only — best for long transcripts"
                }
                className={cn(
                  "btn text-[10px]",
                  synthesisOnly ? "btn-primary" : "border-amber/40 text-amber",
                )}
              >
                {synthesisOnly ? (
                  <Layers className="w-3.5 h-3.5" />
                ) : (
                  <ScrollText className="w-3.5 h-3.5" />
                )}
                {synthesisOnly ? "Full record" : "Ruling only"}
              </button>
            </>
          }
        >
          {lineage && (lineage.parent || lineage.children.length > 0) && (
            <div className="flex flex-wrap items-center gap-2 mt-2 text-[10px] font-mono text-fg-dim">
              <GitBranch className="w-3 h-3 text-amber shrink-0" />
              {lineage.parent && (
                <span>
                  forked from{" "}
                  <span className="text-fg-muted">{lineage.parent.parent_id}</span>
                </span>
              )}
              {lineage.children.length > 0 && (
                <span>
                  {lineage.children.length} child fork
                  {lineage.children.length === 1 ? "" : "s"}
                </span>
              )}
            </div>
          )}
        </ProceedingRecordHead>

        <ProceedingMetrics
          rounds={detail.rounds.length}
          tokens={detail.total_tokens}
          costUsd={detail.total_cost_usd}
          latencyMs={detail.total_latency_ms}
          convergence={finalConv}
        />

        {!synthesisOnly && (
          <>
            <ProceedingPhaseRail phases={phases} />

            {detail.rounds.map((r) => (
              <HistoryRoundLedger key={r.round_num} round={r} />
            ))}
          </>
        )}
      </div>

      <ProceedingRulingColumn
        synthesis={detail.synthesis}
        synthesisModel={detail.synthesis_model}
        sessionId={detail.session_id}
        confidence={entry.confidence}
        placeholder="No synthesis recorded."
      />
    </>
  );
}
