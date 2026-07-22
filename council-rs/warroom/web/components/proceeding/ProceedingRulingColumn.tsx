"use client";

import { type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Loader2 } from "lucide-react";
import { cn } from "@/lib/cn";

export function ProceedingRulingColumn({
  synthesis,
  synthesisModel,
  sessionId,
  confidence,
  placeholder = "No ruling filed.",
  loading,
  loadingLabel = "Chair composing ruling…",
  headerActions,
  footer,
  awaiting,
}: {
  synthesis?: string;
  synthesisModel?: string;
  sessionId?: string;
  confidence?: string;
  placeholder?: string;
  loading?: boolean;
  loadingLabel?: string;
  headerActions?: ReactNode;
  footer?: ReactNode;
  /** Idle shell — faint watermark when no ruling yet. */
  awaiting?: boolean;
}) {
  return (
    <aside className="cg-record-ruling" aria-label="Council ruling">
      <div className="cg-record-ruling-scroll">
        <article className={cn("cg-ruling-card", awaiting && !synthesis && "cg-ruling-card--awaiting")}>
          <div className="cg-ruling-kicker">
            <span>Council ruling</span>
            <div className="flex items-center gap-1.5">
              {synthesisModel && (
                <span className="chip chip-amber text-[9px]">{synthesisModel}</span>
              )}
              {headerActions}
            </div>
          </div>
          {loading && !synthesis && (
            <div className="flex items-center gap-2 py-6 text-sm font-mono text-fg-muted">
              <Loader2 className="w-4 h-4 animate-spin text-amber shrink-0" />
              {loadingLabel}
            </div>
          )}
          {synthesis ? (
            <div className="ruling ruling--column max-w-none text-[13px]">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{synthesis}</ReactMarkdown>
            </div>
          ) : !loading ? (
            <p className="text-sm text-fg-muted font-mono leading-relaxed">{placeholder}</p>
          ) : null}
        </article>
        {(sessionId || confidence) && (
          <div className="mt-2.5 border border-border rounded bg-bg-elevated divide-y divide-border text-[10px] font-mono">
            {sessionId && (
              <div className="flex justify-between px-2.5 py-2 text-fg-muted">
                <span>Session</span>
                <span className="text-fg-muted truncate max-w-[180px]">{sessionId}</span>
              </div>
            )}
            {confidence && (
              <div className="flex justify-between px-2.5 py-2 text-fg-muted">
                <span>Confidence</span>
                <span>{confidence}</span>
              </div>
            )}
          </div>
        )}
        {footer && <div className={cn("mt-3")}>{footer}</div>}
      </div>
    </aside>
  );
}
