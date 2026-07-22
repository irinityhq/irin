"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { motion } from "framer-motion";
import { Loader2, Play, RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import type { MetaReviewReport, MetaReviewResult } from "@/lib/types";

function isMissingReport(error: unknown): boolean {
  return error instanceof Error && /^404\b/.test(error.message);
}

export default function MetaReviewView() {
  const [report, setReport] = useState<MetaReviewReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [running, setRunning] = useState(false);
  const [lastRun, setLastRun] = useState<MetaReviewResult | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [runError, setRunError] = useState<string | null>(null);
  const loadRequestId = useRef(0);

  const loadLatest = useCallback(async () => {
    const requestId = ++loadRequestId.current;
    setLoading(true);
    setLoadError(null);
    try {
      const r = await api.metaReviewLatest();
      if (requestId !== loadRequestId.current) return;
      setReport(r);
    } catch (error) {
      if (requestId !== loadRequestId.current) return;
      setReport(null);
      if (!isMissingReport(error)) {
        setLoadError(
          error instanceof Error ? error.message : "Meta-review request failed",
        );
      }
    } finally {
      if (requestId === loadRequestId.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadLatest();
  }, [loadLatest]);

  const triggerRun = async () => {
    setRunning(true);
    setRunError(null);
    setLastRun(null);
    try {
      const r = await api.metaReviewRun();
      setLastRun(r);
      if (r.status === "error" || r.status === "write_failed") {
        setRunError(r.error ?? "Meta-review failed");
      } else {
        await loadLatest();
      }
    } catch (e) {
      setRunError(e instanceof Error ? e.message : "Meta-review failed");
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="space-y-5">
      <header className="flex items-start justify-between gap-4 border-b border-border pb-3">
        <div className="min-w-0">
          <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
            Self-audit loop
          </div>
          <h1 className="mt-1 text-base font-semibold tracking-tight text-fg">
            Meta-review
          </h1>
          <p className="mt-1 max-w-xl text-[11px] leading-relaxed text-fg-muted">
            Reads weekly drift + intervention history, recommends one parameter
            to tune.
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <button onClick={loadLatest} className="btn text-xs">
            <RotateCcw className="w-3.5 h-3.5" /> Refresh
          </button>
          <button
            onClick={triggerRun}
            disabled={running}
            className="btn btn-primary text-xs"
          >
            {running ? (
              <><Loader2 className="w-3.5 h-3.5 animate-spin" /> Running…</>
            ) : (
              <><Play className="w-3.5 h-3.5" /> Run meta-review</>
            )}
          </button>
        </div>
      </header>

      {runError && (
        <div
          role="alert"
          className="flex flex-wrap items-center justify-between gap-3 rounded border border-danger/40 bg-danger/5 px-3.5 py-2.5"
        >
          <div>
            <div className="font-mono text-xs font-semibold text-danger">
              Meta-review run failed
            </div>
            <div className="mt-1 font-mono text-[10px] text-fg-dim">{runError}</div>
          </div>
          <button type="button" className="btn text-xs" onClick={() => void triggerRun()}>
            Retry run
          </button>
        </div>
      )}

      {lastRun && (
        <div className={cn(
          "flex flex-wrap items-center gap-x-4 gap-y-1.5 rounded border bg-bg-elevated px-3.5 py-2.5 text-[11px] font-mono",
          lastRun.status === "error" || lastRun.status === "write_failed"
            ? "border-danger/40 text-danger"
            : lastRun.status === "insufficient_data" || lastRun.status === "no_drift_data"
              ? "border-warning/40 text-fg-muted"
              : "border-border text-fg-muted",
        )}>
          <span className="text-[10px] font-semibold uppercase tracking-widest text-amber">
            {lastRun.status.replace(/_/g, " ")}
          </span>
          {lastRun.weeks != null && (
            <span className="tabular-nums">{lastRun.weeks} weeks analyzed</span>
          )}
          {lastRun.mean_drift != null && (
            <span className="tabular-nums">avg drift {lastRun.mean_drift.toFixed(3)}</span>
          )}
          {lastRun.stability && (
            <span className="text-fg-dim">{lastRun.stability}</span>
          )}
          {lastRun.recommendation_preview && (
            <span className="max-w-md truncate text-fg-dim">
              {lastRun.recommendation_preview}
            </span>
          )}
        </div>
      )}

      <section className="max-h-[80vh] overflow-y-auto rounded border border-border bg-bg-elevated p-6">
        {loading && (
          <div className="font-mono text-sm text-fg-dim">Loading…</div>
        )}
        {!loading && loadError && (
          <div role="alert" className="space-y-3">
            <div className="font-mono text-sm font-semibold text-danger">
              Could not load latest meta-review
            </div>
            <div className="font-mono text-xs text-fg-dim">{loadError}</div>
            <button type="button" className="btn text-xs" onClick={() => void loadLatest()}>
              Retry latest
            </button>
          </div>
        )}
        {!report && !running && !loading && !loadError && (
          <div className="font-mono text-sm text-fg-dim">
            No meta-review report yet. Run one to analyze your drift signal
            quality and get a tuning recommendation.
          </div>
        )}
        {report && (
          <motion.article
            key={report.name}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            className="ruling max-w-none"
          >
            <div className="mb-4 font-mono text-[10px] uppercase tracking-widest text-fg-dim">
              {report.name} · {report.mtime.slice(0, 16).replace("T", " ")}
            </div>
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {report.content}
            </ReactMarkdown>
          </motion.article>
        )}
      </section>
    </div>
  );
}
