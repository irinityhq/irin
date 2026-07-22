"use client";

import { useState } from "react";
import { ChevronDown, ChevronRight, Shield } from "lucide-react";
import { cn } from "@/lib/cn";
import type { RoundValidationData } from "@/lib/types";

export function ValidationStrip({
  validation,
  variant = "default",
}: {
  validation: RoundValidationData;
  variant?: "default" | "ledger";
}) {
  const [expanded, setExpanded] = useState(false);
  const counts = {
    supported: validation.verdicts.filter((v) => v.verdict === "SUPPORTED").length,
    consistent: validation.verdicts.filter((v) => v.verdict === "CONSISTENT").length,
    no_evidence: validation.verdicts.filter((v) => v.verdict === "NO_EVIDENCE").length,
    contradicted: validation.verdicts.filter((v) => v.verdict === "CONTRADICTED").length,
  };
  const hasContradictions = counts.contradicted > 0;

  return (
    <div
      className={cn(
        "cg-validation",
        variant === "ledger" && "ledger",
        hasContradictions && "has-contra",
      )}
    >
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        title="Sheldon validation — ✓ supported · ~ consistent · ? no evidence · ✗ contradicted"
        className="cg-validation-toggle"
      >
        <Shield
          className={cn(
            "w-3.5 h-3.5 shrink-0",
            hasContradictions ? "text-danger" : "text-fg-dim",
          )}
        />
        <span className="text-success" title="Supported">
          ✓{counts.supported}
        </span>
        <span className="text-fg-dim" title="Consistent">
          ~{counts.consistent}
        </span>
        <span className="text-fg-dim" title="No evidence">
          ?{counts.no_evidence}
        </span>
        {counts.contradicted > 0 && (
          <span className="text-danger" title="Contradicted">
            ✗{counts.contradicted}
          </span>
        )}
        {variant === "ledger" && (
          <span className="text-fg-dim text-[9px] font-normal normal-case tracking-normal ml-0.5 hidden lg:inline">
            supported · consistent · no evidence
            {counts.contradicted > 0 ? " · contradicted" : ""}
          </span>
        )}
        {validation.gate_applied && (
          <span className="chip chip-danger text-[10px]">GATED</span>
        )}
        <span className="ml-auto text-fg-dim">
          {expanded ? (
            <ChevronDown className="w-3 h-3" />
          ) : (
            <ChevronRight className="w-3 h-3" />
          )}
        </span>
      </button>
      {expanded && (
        <div className="cg-validation-body">
          {validation.verdicts.map((v, i) => (
            <div
              key={i}
              className={cn(
                "cg-verdict",
                v.verdict === "SUPPORTED" && "cg-verdict-supported",
                v.verdict === "CONTRADICTED" && "cg-verdict-contradicted",
                v.verdict === "CONSISTENT" && "cg-verdict-consistent",
                v.verdict === "NO_EVIDENCE" && "cg-verdict-no-evidence",
              )}
            >
              <div className="cg-verdict-bar" aria-hidden />
              <div>
                <div className="flex flex-wrap items-center gap-1.5 mb-1">
                  <span
                    className={cn(
                      "cg-verdict-tag",
                      v.verdict === "SUPPORTED" && "cg-verdict-tag-supported",
                      v.verdict === "CONTRADICTED" && "cg-verdict-tag-contradicted",
                      v.verdict === "CONSISTENT" && "cg-verdict-tag-consistent",
                      v.verdict === "NO_EVIDENCE" && "cg-verdict-tag-no-evidence",
                    )}
                  >
                    {v.verdict}
                  </span>
                  <span className="chip text-[10px]">{v.seat}</span>
                  {v.impact === "HIGH" && (
                    <span className="chip chip-danger text-[10px]">HIGH</span>
                  )}
                </div>
                <div className="text-fg text-[11px]">{v.claim}</div>
                {v.reasoning && (
                  <div className="text-fg-dim mt-0.5 text-[10px]">{v.reasoning}</div>
                )}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
