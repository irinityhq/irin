"use client";

import { useState, useRef, KeyboardEvent } from "react";
import { Send, Loader2, Square } from "lucide-react";
import type { LibrarianCabinet } from "@/lib/librarian";

export default function LibrarianComposer({
  cabinets, cabinet, onCabinetChange, onSend, onStop, busy, disabled,
}: {
  cabinets: LibrarianCabinet[];
  cabinet: string;
  onCabinetChange: (c: string) => void;
  onSend: (content: string) => void;
  /** feature contract: abort the in-flight ask (Stop button shows while busy). */
  onStop?: () => void;
  busy: boolean;
  disabled: boolean;
}) {
  const [draft, setDraft] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);

  function handleKey(e: KeyboardEvent<HTMLTextAreaElement>) {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      submit();
    }
  }

  function submit() {
    const text = draft.trim();
    if (!text || busy || disabled) return;
    onSend(text);
    setDraft("");
    ref.current?.focus();
  }

  return (
    <div className="border-t border-border p-3">
      <div className="cg-convene-topic-wrap">
        <span className="cg-convene-matter-infield">Ask the librarian</span>
        <textarea
          ref={ref}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={handleKey}
          rows={2}
          disabled={busy || disabled}
          placeholder="Ask the librarian… (Cmd/Ctrl+Enter to send)"
          className="cg-convene-topic"
        />
        <div className="flex items-center justify-between gap-2 px-3 pb-2.5 pt-1 border-t border-border/50">
          <label className="flex items-center gap-2 min-w-0">
            <span className="label mb-0 shrink-0">Cabinet</span>
            <select
              value={cabinet}
              onChange={(e) => onCabinetChange(e.target.value)}
              disabled={busy || disabled}
              className="min-w-0 bg-bg-deep border border-border rounded px-2 py-1 text-[11px] font-mono text-fg-muted focus:outline-none focus:border-amber/50 disabled:opacity-40"
            >
              {cabinets.map((c) => (
                <option key={c.name} value={c.name}>{c.name}</option>
              ))}
            </select>
          </label>
          {busy && onStop ? (
            <button
              onClick={onStop}
              className="btn btn-danger shrink-0"
              aria-label="Stop"
              data-testid="librarian-stop"
              title="Abort the in-flight ask"
            >
              <Square className="w-4 h-4" />
              Stop
            </button>
          ) : (
            <button
              onClick={submit}
              disabled={!draft.trim() || busy || disabled}
              className="btn btn-primary shrink-0"
              aria-label="Send"
              data-testid="librarian-send"
            >
              {busy ? <Loader2 className="w-4 h-4 animate-spin"/> : <Send className="w-4 h-4"/>}
              Send
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
