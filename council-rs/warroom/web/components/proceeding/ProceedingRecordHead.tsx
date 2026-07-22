"use client";

import { useState, type ReactNode } from "react";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/cn";
import { proceedingTitle, proceedingTopicIsLong } from "@/lib/proceeding-display";
import type { ExecutionRoute } from "@/lib/types";
import { ExecutionRouteChip, RecordModeChip } from "./ModeChips";

export function ProceedingRecordHead({
  mode,
  cabinetLabel,
  topic,
  sessionId,
  executionRoute,
  gatewaySensitivity,
  actions,
  kicker = "Proceeding record",
  children,
}: {
  mode: string;
  cabinetLabel: string;
  topic: string;
  sessionId?: string;
  executionRoute?: ExecutionRoute;
  gatewaySensitivity?: string | null;
  actions?: ReactNode;
  kicker?: string;
  /** Extra rows under the title (e.g. fork lineage in History). */
  children?: ReactNode;
}) {
  const [matterOpen, setMatterOpen] = useState(false);
  const title = proceedingTitle(topic);
  const topicLong = proceedingTopicIsLong(topic);

  return (
    <div className="cg-record-head">
      <div className="grid grid-cols-1 lg:grid-cols-[1fr_auto] gap-3 w-full">
        <div className="min-w-0">
          <div className="cg-record-kicker">
            <span className="text-fg-dim">
              <em className="text-amber not-italic font-semibold">{kicker}</em>
            </span>
            <span className="text-fg-dim/60" aria-hidden>
              /
            </span>
            <RecordModeChip mode={mode} />
            {executionRoute && executionRoute !== "unknown" && (
              <>
                <span className="text-fg-dim/60" aria-hidden>
                  /
                </span>
                <ExecutionRouteChip route={executionRoute} sensitivity={gatewaySensitivity} />
              </>
            )}
            <span className="text-fg-dim/60" aria-hidden>
              /
            </span>
            <span className="chip text-[9px] normal-case tracking-normal font-medium">
              {cabinetLabel}
            </span>
            {sessionId && (
              <>
                <span className="text-fg-dim/60" aria-hidden>
                  /
                </span>
                <span className="text-fg-dim tabular-nums">{sessionId.slice(0, 12)}</span>
              </>
            )}
          </div>
          <h1>{title}</h1>
          {topicLong && (
            <div className="mt-1">
              <button
                type="button"
                onClick={() => setMatterOpen((v) => !v)}
                aria-expanded={matterOpen}
                className="cg-matter-toggle"
              >
                <ChevronRight
                  className={cn(
                    "w-3 h-3 shrink-0 transition-transform duration-150",
                    matterOpen && "rotate-90",
                  )}
                />
                {matterOpen
                  ? "Collapse proceeding statement"
                  : "View full proceeding statement"}
              </button>
              {matterOpen && (
                <div className="cg-matter-full" aria-label="Full proceeding statement">
                  {topic}
                </div>
              )}
            </div>
          )}
          {children}
        </div>
        {actions && (
          <div className="flex flex-wrap items-start gap-1.5 justify-end">{actions}</div>
        )}
      </div>
    </div>
  );
}
