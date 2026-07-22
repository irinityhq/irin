"use client";

import { motion, AnimatePresence } from "framer-motion";
import { History, Sparkles } from "lucide-react";
import type { PrecedentMatchSemantic } from "@/lib/types";

export default function PrecedentAmbient({
  matches,
  blind,
  mode = "keyword",
  variant = "default",
}: {
  matches: PrecedentMatchSemantic[];
  blind: boolean;
  mode?: "semantic" | "keyword";
  variant?: "default" | "command";
}) {
  const shell = variant === "command";
  // Unified-retriever fused score (hybrid-v1 / keyword-v1) preferred; legacy
  // pure-cosine `similarity` kept as fallback for old payloads.
  const relevance = (m: PrecedentMatchSemantic) => m.score ?? m.similarity;
  const wrap = shell
    ? "cg-command-panel cg-command-panel--tight"
    : "panel p-5";
  const blindWrap = shell
    ? "cg-command-panel border-cyan/30 bg-cyan/[0.04]"
    : "panel p-5 border-cyan/40 bg-cyan/5";

  if (blind) {
    return (
      <div className={blindWrap}>
        <div className="flex items-center gap-2 mb-2">
          <History className="w-4 h-4 text-cyan" />
          <span className={shell ? "cg-section-label mb-0" : "label text-cyan"}>
            Blind mode
          </span>
        </div>
        <div className="text-[11px] text-fg-muted font-mono leading-relaxed">
          Precedent search disabled. The council deliberates cold.
        </div>
      </div>
    );
  }

  return (
    <div className={wrap}>
      <div className="flex items-center gap-2 mb-1">
        <History className="w-3.5 h-3.5 text-amber" />
        <span className={shell ? "cg-section-label mb-0" : "label"}>Precedent</span>
        {mode === "semantic" && (
          <span className="chip chip-cyan flex items-center gap-1">
            <Sparkles className="w-2.5 h-2.5" /> semantic
          </span>
        )}
        {matches.length > 0 && (
          <span className="chip chip-amber ml-auto">{matches.length}</span>
        )}
      </div>
      {matches.length === 0 ? (
        <div className="text-[11px] text-fg-dim font-mono leading-relaxed">
          Type a topic — matching prior rulings appear here.
        </div>
      ) : shell ? (
        <div className="space-y-1">
          {matches.slice(0, 5).map((m) => (
            <div
              key={m.id}
              className="border-l-2 border-amber/50 pl-2 py-0.5"
            >
              <div className="text-[10px] font-mono text-amber/80 flex items-center gap-1">
                <span className="truncate">{m.id}</span>
                {relevance(m) != null && (
                  <span
                    className="ml-auto text-cyan tabular-nums"
                    title={m.why}
                  >
                    {Math.round((relevance(m) as number) * 100)}%
                  </span>
                )}
              </div>
              <div className="text-[11px] text-fg-muted leading-snug line-clamp-2">
                {m.topic}
              </div>
            </div>
          ))}
        </div>
      ) : (
        <AnimatePresence>
          <div className="space-y-2">
            {matches.slice(0, 5).map((m) => (
              <motion.div
                key={m.id}
                initial={{ opacity: 0, x: -8 }}
                animate={{ opacity: 1, x: 0 }}
                className="border-l-2 border-amber/50 pl-3 py-1"
              >
                <div className="text-[10px] font-mono text-amber/80 flex items-center gap-1">
                  {m.id} · {m.cabinet} · conf {m.confidence}
                  {relevance(m) != null && (
                    <span className="ml-auto text-cyan" title={m.why}>
                      {Math.round((relevance(m) as number) * 100)}%
                    </span>
                  )}
                </div>
                {m.why && (
                  <div className="text-[10px] text-fg-dim font-mono">
                    {m.why}
                  </div>
                )}
                <div className="text-xs text-fg leading-snug line-clamp-2">
                  {m.topic}
                </div>
              </motion.div>
            ))}
          </div>
        </AnimatePresence>
      )}
    </div>
  );
}
