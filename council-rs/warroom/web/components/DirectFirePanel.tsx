"use client";

import { useState } from "react";
import { Copy, Crosshair, Download, Loader2, RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useDeliberation } from "@/hooks/useDeliberation";
import { cn, fmtCost, fmtLatency, fmtTokens } from "@/lib/cn";
import { DIRECT_FIRE_MODES, buildDirectFireStartPayload } from "@/lib/direct-fire";
import type { DirectFireMode } from "@/lib/ws";
import ContextUploader from "./ContextUploader";
import ExperimentalBanner from "./ExperimentalBanner";

/**
 * Single-shot direct fire (feature contract) — CLI parity for `--contrarian`,
 * `--munger`, `--kiss-review`, `--specops`, `--premortem`. One model, no
 * council rounds: the WS stream is `session_started → synthesis_started →
 * synthesis_complete → session_saved → done`, so this panel renders only a
 * firing status and the final synthesis (never round/seat cards).
 *
 * Skin: command-grade intake — this is a sibling of the convene form, but the
 * output is a single model talking, not a filed council ruling, so the verdict
 * body stays plain Inter prose (no authority serif).
 */
export default function DirectFirePanel() {
  // Own deliberation instance — the Deliberate view's instance keeps its
  // reconnect/abort affordances; direct fire is short-lived and self-reset.
  const { state, start, reset } = useDeliberation();
  const [topic, setTopic] = useState("");
  const [context, setContext] = useState("");
  const [mode, setMode] = useState<DirectFireMode>("contrarian");
  const [copied, setCopied] = useState(false);

  const modeInfo = DIRECT_FIRE_MODES.find((m) => m.mode === mode)!;
  const canFire = topic.trim().length > 4;
  const firing =
    state.phase === "connecting" ||
    state.phase === "streaming" ||
    state.phase === "paused" ||
    state.phase === "synthesizing";
  const isDone = state.phase === "done" && !!state.synthesis;

  const fire = () => {
    if (!canFire) return;
    start(
      buildDirectFireStartPayload({
        topic,
        mode,
        context: context.trim() || undefined,
      }),
    );
  };

  const copy = async () => {
    if (!state.synthesis) return;
    await navigator.clipboard.writeText(state.synthesis.text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const downloadMd = () => {
    if (!state.synthesis) return;
    const md = [
      `# Direct Fire — ${modeInfo.label} — ${state.session_id}`,
      ``,
      `**Topic:** ${topic.trim()}`,
      `**Model:** ${state.synthesis.model}`,
      ``,
      `---`,
      ``,
      state.synthesis.text,
    ].join("\n");
    const blob = new Blob([md], { type: "text/markdown" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `direct_fire_${mode}_${state.session_id || "result"}.md`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="max-w-4xl mx-auto space-y-5" data-testid="direct-fire-panel">
      {state.phase === "idle" && (
        <>
          <div className="space-y-2">
            <p className="cg-section-label mb-0">
              <Crosshair className="w-3.5 h-3.5 text-amber" />
              Direct fire — single shot
            </p>
            <div className="cg-convene-topic-wrap">
              <label
                htmlFor="direct-fire-topic-input"
                className="cg-convene-matter-infield"
              >
                The matter
              </label>
              <textarea
                id="direct-fire-topic-input"
                data-testid="direct-fire-topic"
                value={topic}
                onChange={(e) => setTopic(e.target.value)}
                placeholder="Single model, single shot — no council rounds. State what to attack, invert, simplify, filter, or premortem…"
                rows={3}
                className="cg-convene-topic"
                autoFocus
                aria-label="Direct fire statement"
              />
            </div>
            <div className="cg-convene-meta">
              <span>{topic.length} chars</span>
            </div>
          </div>

          <div className="space-y-2.5">
            <p className="cg-section-label mb-0">Fire mode</p>
            <div className="grid grid-cols-2 md:grid-cols-5 gap-2">
              {DIRECT_FIRE_MODES.map((m) => (
                <button
                  key={m.mode}
                  type="button"
                  data-testid={`direct-fire-mode-${m.mode}`}
                  onClick={() => setMode(m.mode)}
                  className={cn(
                    "text-left p-2.5 rounded border transition-colors",
                    mode === m.mode
                      ? "border-amber/60 bg-amber/[0.06]"
                      : "border-border bg-bg-deep/40 hover:border-border-bright",
                  )}
                >
                  <div
                    className={cn(
                      "text-xs font-mono font-semibold",
                      mode === m.mode ? "text-amber" : "text-fg",
                    )}
                  >
                    {m.label}
                  </div>
                  <div className="text-[10px] font-mono text-fg-dim mt-0.5 leading-snug">
                    {m.description}
                  </div>
                </button>
              ))}
            </div>
            <p className="text-[10px] font-mono text-fg-dim">
              CLI parity: <code className="text-cyan">{modeInfo.cliFlag}</code>
            </p>
          </div>

          {modeInfo.experimental && (
            <ExperimentalBanner
              title="Experimental premortem"
              testId="premortem-experimental"
            >
              <p>
                Premortem assumes the plan has already failed and works
                backwards to the causes. It is an experimental mode with
                explicit kill criteria — review the doc below before relying
                on its output.
              </p>
            </ExperimentalBanner>
          )}

          <ContextUploader value={context} onChange={setContext} />

          <button
            data-testid="direct-fire-submit"
            onClick={fire}
            disabled={!canFire}
            className="btn btn-primary w-full justify-center py-3"
          >
            <Crosshair className="w-4 h-4" />
            Fire {modeInfo.label}
          </button>
        </>
      )}

      {firing && (
        <div
          data-testid="direct-fire-firing"
          className="panel p-12 text-center font-mono text-sm text-fg-muted space-y-3"
        >
          <Loader2 className="w-6 h-6 animate-spin mx-auto text-fg-dim" />
          <div>{modeInfo.label} firing — single shot, no rounds…</div>
        </div>
      )}

      {isDone && state.synthesis && (
        <>
          <div className="panel p-5">
            <div className="flex items-center justify-between border-b border-border pb-2.5 mb-4">
              <div className="flex items-center gap-2">
                <Crosshair className="w-4 h-4 text-amber" />
                <span className="text-[11px] font-mono font-semibold uppercase tracking-widest text-amber">
                  {modeInfo.label} verdict
                </span>
                <span className="chip chip-amber">{state.synthesis.model}</span>
              </div>
              <div className="flex items-center gap-2">
                <button onClick={() => void copy()} className="btn text-[10px]">
                  <Copy className="w-3.5 h-3.5" /> {copied ? "Copied" : "Copy"}
                </button>
                <button onClick={downloadMd} className="btn text-[10px]">
                  <Download className="w-3.5 h-3.5" /> .md
                </button>
              </div>
            </div>
            <article
              data-testid="direct-fire-result"
              className={cn(
                "max-w-none font-sans text-[13px] leading-relaxed text-fg",
                "[&_p]:my-2 [&_h1]:text-base [&_h1]:font-semibold [&_h1]:mt-4 [&_h1]:mb-2",
                "[&_h2]:text-sm [&_h2]:font-semibold [&_h2]:mt-3 [&_h2]:mb-1.5",
                "[&_h3]:text-[13px] [&_h3]:font-semibold [&_h3]:mt-3 [&_h3]:mb-1",
                "[&_ul]:list-disc [&_ul]:pl-5 [&_ul]:my-2 [&_ol]:list-decimal [&_ol]:pl-5 [&_ol]:my-2",
                "[&_li]:my-0.5 [&_strong]:text-fg-bright [&_strong]:font-semibold",
                "[&_code]:font-mono [&_code]:text-[12px] [&_code]:text-fg-muted",
                "[&_a]:text-cyan [&_a]:underline",
                "[&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 [&_blockquote]:text-fg-muted",
              )}
            >
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {state.synthesis.text}
              </ReactMarkdown>
            </article>
          </div>

          <div className="cg-metrics rounded border border-border overflow-hidden">
            <div className="cg-metric">
              <span>Tokens</span>
              <b>{fmtTokens(state.totals.tokens)}</b>
            </div>
            <div className="cg-metric">
              <span>Cost</span>
              <b>{fmtCost(state.totals.cost_usd)}</b>
            </div>
            <div className="cg-metric">
              <span>Latency</span>
              <b>{fmtLatency(state.totals.latency_ms)}</b>
            </div>
          </div>

          <div className="flex justify-between items-center">
            <div
              data-testid="direct-fire-saved"
              className="text-[10px] font-mono text-fg-dim"
            >
              {state.saved_path && `Saved → ${state.saved_path}`}
            </div>
            <button onClick={reset} className="btn btn-primary">
              <RotateCcw className="w-4 h-4" />
              New direct fire
            </button>
          </div>
        </>
      )}

      {state.phase === "error" && (
        <div
          role="alert"
          data-testid="direct-fire-error"
          className="panel border-danger/40 bg-danger/5 p-6 space-y-2"
        >
          <div className="text-[11px] font-mono font-semibold uppercase tracking-widest text-danger">
            Direct fire failed
          </div>
          {state.errors.map((e, i) => (
            <div key={i} className="font-mono text-xs text-fg-muted">
              {e.message}
            </div>
          ))}
          <button
            type="button"
            data-testid="direct-fire-retry"
            onClick={reset}
            className="btn btn-primary mt-4"
          >
            Try again
          </button>
        </div>
      )}
    </div>
  );
}
