"use client";

import { useState } from "react";
import { ChevronRight, ChevronDown, AlertTriangle } from "lucide-react";
import ReactMarkdown from "react-markdown";
import type { LibrarianMessage, LibrarianSource } from "@/lib/librarian";
import { safeMarkdownUrl } from "@/lib/markdown-url";

const DISALLOWED = ["script", "iframe", "object", "embed"];

/** R20 — in-flight streaming assistant turn (before it is persisted). */
export interface PendingTurn {
  text: string;
  sources: LibrarianSource[];
  streaming: boolean;
}

/** Quiet mono status line — mirrors the seat "Composing…" treatment. */
function ComposingStatus() {
  return (
    <span className="text-[12px] font-mono flex items-center gap-1.5 text-amber/90 italic">
      <span
        className="inline-block w-1.5 h-1.5 rounded-full bg-current animate-pulse"
        aria-hidden
      />
      Composing…
    </span>
  );
}

export default function LibrarianTranscript({
  messages, busy, pending,
}: {
  messages: LibrarianMessage[];
  busy: boolean;
  /** R20 — streaming assistant turn shown live; null when not streaming. */
  pending?: PendingTurn | null;
}) {
  // While a streaming turn is live we render it instead of the generic
  // "Asking…" placeholder. Zero-chunk flows show the placeholder until the
  // final message lands — identical to the pre-R20 behavior.
  const showPending = !!pending && (pending.streaming || pending.text.length > 0);
  const showPlaceholder = busy && !(pending && pending.text.length > 0);
  return (
    <div className="flex-1 overflow-y-auto px-6 py-2">
      {messages.map((m) => (
        <Turn key={m.id} m={m} />
      ))}
      {showPending && pending && (
        <PendingAssistant pending={pending} />
      )}
      {showPlaceholder && (
        <div className="py-4 border-b border-border">
          <div className="label text-amber/80 mb-1.5">Librarian</div>
          <ComposingStatus />
        </div>
      )}
    </div>
  );
}

/** Live streaming assistant turn — markdown re-parses per chunk (v1). */
function PendingAssistant({ pending }: { pending: PendingTurn }) {
  return (
    <div className="py-4 border-b border-border" data-testid="librarian-pending-turn">
      <div className="label text-amber/80 mb-1.5">Librarian</div>
      {pending.text ? (
        <div className="prose prose-invert prose-sm max-w-none">
          <ReactMarkdown disallowedElements={DISALLOWED} urlTransform={safeMarkdownUrl}>
            {pending.text}
          </ReactMarkdown>
          {pending.streaming && (
            <span
              data-testid="librarian-stream-cursor"
              aria-hidden
              className="inline-block w-[2px] h-3.5 ml-0.5 align-text-bottom bg-amber animate-pulse"
            />
          )}
        </div>
      ) : (
        <ComposingStatus />
      )}
      {pending.sources.length > 0 && (
        <div className="pt-2 mt-2 border-t border-border text-[10px] font-mono text-fg-dim">
          {pending.sources.length} source
          {pending.sources.length === 1 ? "" : "s"}
        </div>
      )}
    </div>
  );
}

function Turn({ m }: { m: LibrarianMessage }) {
  const [showSources, setShowSources] = useState(false);
  if (m.type === "user") {
    return (
      <div className="py-4 border-b border-border">
        <div className="label mb-1.5">Operator</div>
        <div className="text-[13px] font-sans leading-[1.55] text-fg whitespace-pre-wrap">
          {m.content}
        </div>
      </div>
    );
  }
  return (
    <div className="py-4 border-b border-border">
      <div className="flex items-center gap-2 mb-1.5">
        <span className="label text-amber/80">Librarian</span>
        {m.redacted && (
          <span className="text-[10px] font-mono text-amber flex items-center gap-1">
            <AlertTriangle className="w-3 h-3" />
            content was redacted (secret-shaped)
          </span>
        )}
      </div>
      <div className="prose prose-invert prose-sm max-w-none">
        <ReactMarkdown
          disallowedElements={DISALLOWED}
          urlTransform={safeMarkdownUrl}
        >
          {m.content}
        </ReactMarkdown>
      </div>
      {m.sources && m.sources.length > 0 && (
        <div className="pt-2 mt-2 border-t border-border">
          <button
            onClick={() => setShowSources((s) => !s)}
            className="text-[10px] font-mono text-fg-dim flex items-center gap-1 hover:text-fg transition-colors"
          >
            {showSources ? <ChevronDown className="w-3 h-3"/> : <ChevronRight className="w-3 h-3"/>}
            {m.sources.length} source{m.sources.length === 1 ? "" : "s"}
          </button>
          {showSources && (
            <ul className="mt-2 space-y-1 text-xs font-mono text-fg-muted">
              {m.sources.map((s, i) => (
                <li key={i} className="border-l-2 border-amber/40 pl-2">
                  <div className="text-fg flex items-center gap-1.5 flex-wrap">
                    <span>{s.path}</span>
                    {s.corpus && (
                      <span
                        data-testid="source-corpus-badge"
                        className={`chip text-[10px] ${s.corpus === "identity" ? "chip-magenta" : "chip-cyan"}`}
                        title={`Source corpus: ${s.corpus}`}
                      >
                        {s.corpus}
                      </span>
                    )}
                    {typeof s.trust_tier === "number" && (
                      <span
                        data-testid="source-trust-badge"
                        className="chip chip-amber text-[10px]"
                        title={`Trust tier ${s.trust_tier}`}
                      >
                        T{s.trust_tier}
                      </span>
                    )}
                  </div>
                  <div className="text-fg-dim">score {s.score.toFixed(3)}</div>
                  {s.snippet && (
                    <div className="mt-1 prose prose-invert prose-xs max-w-none">
                      <ReactMarkdown
                        disallowedElements={DISALLOWED}
                        urlTransform={safeMarkdownUrl}
                      >
                        {s.snippet}
                      </ReactMarkdown>
                    </div>
                  )}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
