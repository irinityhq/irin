"use client";

import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import { ArrowLeftRight, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import type { SynthesisDiffResult } from "@/lib/types";

export default function SynthesisDiff({
  parentId,
  childId,
  onClose,
}: {
  parentId: string;
  childId: string;
  onClose: () => void;
}) {
  const [diff, setDiff] = useState<SynthesisDiffResult | null>(null);
  const [showText, setShowText] = useState(false);

  useEffect(() => {
    api.diff(parentId, childId).then(setDiff).catch(() => {});
  }, [parentId, childId]);

  if (!diff) {
    return (
      <div className="panel p-12 text-center font-mono text-cyan animate-pulse-cyan">
        Computing diff…
      </div>
    );
  }

  const driftPct = Math.round(diff.drift * 100);
  const tone = driftPct < 20 ? "success" : driftPct < 40 ? "warning" : "danger";

  return (
    <motion.div
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      className="space-y-4"
    >
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <ArrowLeftRight className="w-5 h-5 text-amber" />
          <span className="font-display font-bold text-xl text-fg-bright">
            Synthesis diff
          </span>
        </div>
        <button onClick={onClose} className="btn text-xs">
          <X className="w-3.5 h-3.5" /> Close
        </button>
      </div>

      <div className="panel p-5 grid grid-cols-2 md:grid-cols-5 gap-4">
        <Stat
          label="Drift"
          value={`${driftPct}%`}
          tone={tone}
          sub="0=identical · 1=divergent"
        />
        <Stat
          label="Cosine sim"
          value={diff.similarity != null ? diff.similarity.toFixed(3) : "—"}
          tone="cyan"
        />
        <Stat
          label="Jaccard"
          value={diff.jaccard.toFixed(3)}
          tone="muted"
        />
        <Stat
          label="Confidence"
          value={
            diff.parent_confidence === diff.child_confidence
              ? diff.parent_confidence ?? "—"
              : `${diff.parent_confidence ?? "?"} → ${diff.child_confidence ?? "?"}`
          }
          tone={diff.confidence_changed ? "magenta" : "success"}
        />
        <Stat
          label="Word count"
          value={`${diff.parent_word_count} → ${diff.child_word_count}`}
          tone="muted"
        />
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
        <div className="panel p-5">
          <div className="flex items-center gap-2 mb-3">
            <span className="chip">parent</span>
            <span className="font-mono text-xs text-amber">{diff.parent_id}</span>
          </div>
          <article className="ruling text-sm max-h-96 overflow-y-auto">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {diff.parent_synthesis}
            </ReactMarkdown>
          </article>
        </div>
        <div className="panel p-5">
          <div className="flex items-center gap-2 mb-3">
            <span className="chip chip-amber">child (fork)</span>
            <span className="font-mono text-xs text-amber">{diff.child_id}</span>
          </div>
          <article className="ruling text-sm max-h-96 overflow-y-auto">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {diff.child_synthesis}
            </ReactMarkdown>
          </article>
        </div>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <div className="panel p-5">
          <div className="label mb-2">Unique to parent</div>
          <div className="flex flex-wrap gap-1.5">
            {diff.unique_to_parent.map((w) => (
              <span key={w} className="chip text-[10px]">{w}</span>
            ))}
            {diff.unique_to_parent.length === 0 && (
              <span className="text-fg-dim text-xs font-mono">(none)</span>
            )}
          </div>
        </div>
        <div className="panel p-5">
          <div className="label mb-2 text-amber">Unique to child</div>
          <div className="flex flex-wrap gap-1.5">
            {diff.unique_to_child.map((w) => (
              <span key={w} className="chip chip-amber text-[10px]">{w}</span>
            ))}
            {diff.unique_to_child.length === 0 && (
              <span className="text-fg-dim text-xs font-mono">(none)</span>
            )}
          </div>
        </div>
      </div>

      <div className="panel p-5">
        <button
          onClick={() => setShowText((v) => !v)}
          className="text-xs text-fg-muted hover:text-fg"
        >
          {showText ? "Hide" : "Show"} unified text diff
        </button>
        {showText && (
          <pre className="mt-3 text-[11px] font-mono text-fg-muted whitespace-pre-wrap max-h-96 overflow-y-auto bg-bg-deep p-3 rounded">
            {diff.diff_lines.map((line, i) => (
              <span
                key={i}
                className={cn(
                  "block",
                  line.startsWith("+") && !line.startsWith("+++")
                    ? "text-success"
                    : line.startsWith("-") && !line.startsWith("---")
                      ? "text-danger"
                      : line.startsWith("@@")
                        ? "text-cyan"
                        : "",
                )}
              >
                {line}
              </span>
            ))}
          </pre>
        )}
      </div>
    </motion.div>
  );
}

function Stat({
  label, value, tone, sub,
}: {
  label: string; value: string; sub?: string;
  tone: "amber" | "cyan" | "muted" | "success" | "warning" | "danger" | "magenta";
}) {
  return (
    <div>
      <div className="label">{label}</div>
      <div
        className={cn(
          "font-display font-bold text-2xl tabular-nums mt-1",
          tone === "amber" && "text-amber",
          tone === "cyan" && "text-cyan",
          tone === "muted" && "text-fg",
          tone === "success" && "text-success",
          tone === "warning" && "text-warning",
          tone === "danger" && "text-danger",
          tone === "magenta" && "text-magenta",
        )}
      >
        {value}
      </div>
      {sub && <div className="text-[10px] font-mono text-fg-dim mt-1">{sub}</div>}
    </div>
  );
}
