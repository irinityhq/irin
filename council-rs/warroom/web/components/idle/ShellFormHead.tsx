"use client";

import { useLayoutEffect, useRef } from "react";
import type { Cabinet, PrecedentMatch } from "@/lib/types";
import { RecordModeChip } from "../proceeding/ModeChips";

export function ShellFormHead({
  wireMode,
  cabinet,
  cabinetName,
  topic,
  setTopic,
  blind,
  precedent,
}: {
  wireMode: string;
  cabinet?: Cabinet;
  cabinetName: string;
  topic: string;
  setTopic: (v: string) => void;
  blind: boolean;
  precedent: PrecedentMatch[];
}) {
  const topicRef = useRef<HTMLTextAreaElement>(null);

  useLayoutEffect(() => {
    const el = topicRef.current;
    if (!el) return;
    const cap = Math.floor(window.innerHeight * 0.4);
    el.style.height = "auto";
    const full = el.scrollHeight;
    const next = Math.min(Math.max(full, 72), cap);
    el.style.height = `${next}px`;
    el.style.overflowY = full > cap ? "auto" : "hidden";
  }, [topic]);

  return (
    <div className="cg-record-head cg-convene-head">
      <div className="cg-record-kicker">
        <span className="text-fg-dim">
          <em className="text-amber not-italic font-semibold">File the matter</em>
        </span>
        <span className="text-fg-dim/60" aria-hidden>/</span>
        <RecordModeChip mode={wireMode} />
        <span className="text-fg-dim/60" aria-hidden>/</span>
        <span className="chip text-[9px] normal-case tracking-normal font-medium">
          {cabinet?.label ?? cabinetName}
        </span>
      </div>
      <div className="cg-convene-topic-wrap mt-3">
        <label htmlFor="convene-topic" className="cg-convene-matter-infield">
          The matter
        </label>
        <textarea
          id="convene-topic"
          ref={topicRef}
          value={topic}
          onChange={(e) => setTopic(e.target.value)}
          placeholder="State the question, decision, or proposal the council should deliberate on…"
          rows={2}
          className="cg-convene-topic"
          autoFocus
          aria-label="Proceeding statement"
        />
      </div>
      <div className="cg-convene-meta">
        <span>{topic.length} chars</span>
        {!blind && precedent.length > 0 && (
          <span className="cg-convene-meta-match">
            {precedent.length} prior match{precedent.length === 1 ? "" : "es"}
          </span>
        )}
      </div>
    </div>
  );
}
