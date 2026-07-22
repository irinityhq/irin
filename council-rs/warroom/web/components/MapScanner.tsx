"use client";

import { useEffect, useState } from "react";
import {
  Compass, Folder, Loader2, RefreshCw, Sparkles, Trash2,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "@/lib/api";
import { cn, fmtCost, fmtLatency, fmtTokens } from "@/lib/cn";
import type { MapmakerBriefSummary, MapmakerResult, MapPreview } from "@/lib/types";
import { getProviderOption, useDiscover } from "@/lib/use-discover";

type ModelChoice = "auto" | "grok" | "gemini";

export default function MapScanner({
  value,
  onChange,
  onMapReady,
}: {
  value: string;
  onChange: (v: string) => void;
  onMapReady?: (map: { text: string; result: MapmakerResult } | null) => void;
}) {
  const [preview, setPreview] = useState<MapPreview | null>(null);
  const [probing, setProbing] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const [task, setTask] = useState("");
  const [model, setModel] = useState<ModelChoice>("auto");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<MapmakerResult | null>(null);
  const [open, setOpen] = useState(true);
  const [briefs, setBriefs] = useState<MapmakerBriefSummary[]>([]);
  const [showBriefs, setShowBriefs] = useState(false);
  const [briefContent, setBriefContent] = useState<string | null>(null);
  const { providerOptions } = useDiscover();
  const grokAvailable = getProviderOption(providerOptions, "grok_hermes")?.available === true;
  const geminiAvailable = getProviderOption(providerOptions, "gemini_agy")?.available === true;
  const modelAvailable = (choice: ModelChoice) =>
    choice === "grok"
      ? grokAvailable
      : choice === "gemini"
        ? geminiAvailable
        : grokAvailable && geminiAvailable;

  useEffect(() => {
    api.mapmakerBriefs(20).then((r) => setBriefs(r.briefs)).catch(() => {});
  }, [result]);

  const probe = async () => {
    if (!value.trim()) return;
    setProbing(true); setErr(null);
    try {
      const p = await api.mapPreview(value.trim());
      if (p.error) { setErr(p.error); setPreview(null); }
      else setPreview(p);
    } catch (e) { setErr((e as Error).message); }
    finally { setProbing(false); }
  };

  const runMapmaker = async () => {
    if (!value.trim() || !task.trim()) return;
    setRunning(true); setErr(null);
    try {
      const r = await api.mapmakerRun(value.trim(), task.trim(), model);
      if (r.error) {
        setErr(r.error);
        setResult(null);
        onMapReady?.(null);
      } else {
        setResult(r);
        setOpen(true);
        onMapReady?.({ text: r.map, result: r });
      }
    } catch (e) {
      setErr((e as Error).message);
      setResult(null);
      onMapReady?.(null);
    } finally { setRunning(false); }
  };

  const clear = () => {
    setResult(null);
    onMapReady?.(null);
  };

  const canRun = value.trim().length > 0 && task.trim().length > 0 && !running && modelAvailable(model);

  return (
    <div className="panel p-5 space-y-3">
      <div className="flex items-center gap-2">
        <Folder className="w-4 h-4 text-magenta" />
        <span className="label">Codebase Mapmaker</span>
        <span className="text-[10px] font-mono text-fg-dim ml-auto">
          pre-flight · scope to brief
        </span>
      </div>

      <div className="flex gap-2">
        <input
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder="/path/to/some-project/src"
          className="input text-xs"
        />
        <button
          onClick={probe}
          disabled={!value.trim() || probing}
          className="btn btn-magenta text-xs"
          title="Quick listing of code files in this directory"
        >
          <RefreshCw className={cn("w-3.5 h-3.5", probing && "animate-spin")} />
          Probe
        </button>
      </div>

      {err && <div className="text-xs text-danger font-mono">{err}</div>}

      {preview && !result && (
        <div className="text-xs font-mono space-y-1">
          <div className="text-fg-muted">
            <span className="text-magenta">{preview.file_count}</span> files,{" "}
            <span className="text-magenta">{fmtTokens(preview.total_bytes)}</span>{" "}
            bytes
          </div>
          <div className="max-h-32 overflow-y-auto bg-bg-deep rounded border border-border p-2 space-y-0.5">
            {preview.files.slice(0, 30).map((f) => (
              <div key={f} className="text-fg-dim truncate">{f}</div>
            ))}
            {preview.files.length > 30 && (
              <div className="text-fg-dim">+{preview.files.length - 30} more</div>
            )}
          </div>
        </div>
      )}

      <div className="border-t border-border pt-3 space-y-2">
        <div className="flex items-center gap-2">
          <Compass className="w-3.5 h-3.5 text-amber" />
          <span className="text-xs font-mono text-fg-muted">
            Compress to Execution Map
          </span>
        </div>
        <textarea
          data-testid="mapmaker-task"
          value={task}
          onChange={(e) => setTask(e.target.value)}
          placeholder="What should the executor map in this codebase?"
          rows={2}
          className="input text-xs resize-y"
        />
        {task.trim().length === 0 && (
          <div className="text-[10px] text-fg-dim -mt-1">
            Task is required and stays separate from the matter being deliberated.
          </div>
        )}
        <div className="flex items-center gap-1 flex-wrap">
          <span className="text-[10px] font-mono text-fg-dim mr-1">Model</span>
          {(["auto", "grok", "gemini"] as ModelChoice[]).map((m) => (
            <button
              key={m}
              type="button"
              onClick={() => setModel(m)}
              disabled={!modelAvailable(m)}
              title={!modelAvailable(m) ? `${m} Mapmaker transport is unavailable` : undefined}
              className={cn(
                "chip text-[10px] cursor-pointer transition-colors",
                model === m
                  ? m === "grok" ? "chip-magenta"
                    : m === "gemini" ? "chip-cyan"
                    : "chip-amber"
                  : "opacity-60 hover:opacity-100",
                !modelAvailable(m) && "cursor-not-allowed opacity-30 hover:opacity-30",
              )}
            >
              {m === "auto" ? "auto"
                : m === "grok" ? "Grok via Hermes"
                : "Gemini via agy"}
            </button>
          ))}
          <button
            onClick={runMapmaker}
            disabled={!canRun}
            className={cn(
              "btn btn-amber text-xs ml-auto",
              canRun && "animate-pulse-amber",
            )}
            title="Send the codebase + task to the Mapmaker model and produce a structured Execution Map. The task describes what the executor should map."
          >
            {running ? (
              <><Loader2 className="w-3.5 h-3.5 animate-spin" /> Mapping…</>
            ) : (
              <><Sparkles className="w-3.5 h-3.5" /> Run Mapmaker</>
            )}
          </button>
        </div>
      </div>

      {result && (
        <div className="rounded-md border border-amber/40 bg-amber/5 overflow-hidden">
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="w-full flex items-center gap-2 px-3 py-2 hover:bg-amber/10 transition-colors text-left"
          >
            <Compass className="w-3.5 h-3.5 text-amber shrink-0" />
            <span className="text-xs font-display font-bold text-amber truncate">
              Execution Map ready
            </span>
            <span className="ml-auto flex items-center gap-2 text-[10px] font-mono text-fg-dim">
              <span
                className={cn(
                  "chip text-[10px]",
                  result.model === "grok" ? "chip-magenta" : "chip-cyan",
                )}
              >
                {result.model === "grok" ? "Grok via Hermes" : "Gemini via agy"}
              </span>
              <span>{result.file_count}f</span>
              <span>{fmtTokens(result.tokens_in + result.tokens_out)}</span>
              <span className="text-amber">{fmtCost(result.cost_usd)}</span>
              <span>{fmtLatency(result.latency_ms)}</span>
            </span>
          </button>
          {open && (
            <div className="px-4 pb-3 pt-1 border-t border-amber/20 space-y-2">
              <article className="ruling text-xs max-w-none max-h-[40vh] overflow-y-auto">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>
                  {result.map}
                </ReactMarkdown>
              </article>
              <div className="flex items-center gap-2 text-[10px] font-mono text-fg-dim pt-1 border-t border-amber/20">
                <span>
                  Will be injected as deliberation context (replaces raw{" "}
                  <code>--map</code> dump).
                </span>
                {result.brief_filename && (
                  <span className="ml-auto truncate">
                    saved → {result.brief_filename}
                  </span>
                )}
                <button
                  onClick={clear}
                  className="btn text-[10px] py-1 px-2"
                  title="Discard the map; revert to raw --map injection"
                >
                  <Trash2 className="w-3 h-3" /> Clear
                </button>
              </div>
            </div>
          )}
        </div>
      )}
      {briefs.length > 0 && (
        <div className="border-t border-border pt-3">
          <button
            onClick={() => setShowBriefs((v) => !v)}
            className="flex items-center gap-2 text-xs text-fg-muted hover:text-fg"
          >
            <Folder className="w-3.5 h-3.5" />
            {showBriefs ? "Hide" : "Browse"} past briefs ({briefs.length})
          </button>
          {showBriefs && (
            <div className="mt-2 space-y-1 max-h-48 overflow-y-auto">
              {briefs.map((b) => (
                <button
                  key={b.name}
                  onClick={async () => {
                    try {
                      const full = await api.mapmakerBrief(b.name);
                      setBriefContent(full.content);
                    } catch { setBriefContent(null); }
                  }}
                  className="w-full text-left p-2 rounded text-xs font-mono text-fg-muted hover:bg-bg-overlay"
                >
                  <div className="truncate">{b.name}</div>
                  <div className="text-[10px] text-fg-dim">
                    {b.mtime.slice(0, 16).replace("T", " ")} · {fmtTokens(b.size)} bytes
                  </div>
                </button>
              ))}
            </div>
          )}
          {briefContent && showBriefs && (
            <div className="mt-2 border border-border rounded-md p-3 max-h-60 overflow-y-auto">
              <article className="ruling text-xs max-w-none">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>
                  {briefContent}
                </ReactMarkdown>
              </article>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
