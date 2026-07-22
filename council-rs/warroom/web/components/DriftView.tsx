"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { motion } from "framer-motion";
import { Loader2, Play, RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import type {
  DriftReportListResponse, DriftReport, WeeklySummary,
} from "@/lib/types";
import { useToast } from "./Toast";

function driftTone(v: number): "danger" | "warning" | "success" {
  return v > 0.4 ? "danger" : v > 0.2 ? "warning" : "success";
}

export default function DriftView({
  initialReport,
  onConsumeInitial,
}: {
  initialReport?: string | null;
  onConsumeInitial?: () => void;
} = {}) {
  const [list, setList] = useState<DriftReportListResponse | null>(null);
  const [listLoading, setListLoading] = useState(true);
  const [listError, setListError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [report, setReport] = useState<DriftReport | null>(null);
  const [reportLoading, setReportLoading] = useState(false);
  const [reportError, setReportError] = useState<string | null>(null);
  const [reportRetry, setReportRetry] = useState(0);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);
  const [runStatusUnknown, setRunStatusUnknown] = useState(false);
  const [windowDays, setWindowDays] = useState(7);
  const [limit, setLimit] = useState<number>(8);
  const [weeklyHistory, setWeeklyHistory] = useState<WeeklySummary[]>([]);
  const [weeklyError, setWeeklyError] = useState<string | null>(null);
  const listRequestId = useRef(0);
  const confirmedRunStart = useRef(false);
  const { toast } = useToast();

  const refreshList = useCallback(async (
    showLoading = true,
  ): Promise<"applied" | "failed" | "stale"> => {
    const requestId = ++listRequestId.current;
    if (showLoading) setListLoading(true);
    setListError(null);
    try {
      const r = await api.driftReports();
      if (requestId !== listRequestId.current) return "stale";
      setList(r);
      setRunning(r.running);
      setRunStatusUnknown(false);
      if (!r.running) confirmedRunStart.current = false;
      setSelected((current) =>
        current && r.reports.some((item) => item.name === current)
          ? current
          : r.reports[0]?.name ?? null,
      );
      return "applied";
    } catch (error) {
      if (requestId !== listRequestId.current) return "stale";
      setList(null);
      if (confirmedRunStart.current) {
        setRunStatusUnknown(true);
      } else {
        setRunning(false);
      }
      setListError(error instanceof Error ? error.message : "Drift reports request failed");
      return "failed";
    } finally {
      if (requestId === listRequestId.current) setListLoading(false);
    }
  }, []);

  const refreshWeeklyHistory = useCallback(async () => {
    setWeeklyError(null);
    try {
      const result = await api.weeklyHistory(12);
      setWeeklyHistory(result.summaries);
    } catch (error) {
      setWeeklyHistory([]);
      setWeeklyError(
        error instanceof Error ? error.message : "Weekly drift history request failed",
      );
    }
  }, []);

  useEffect(() => {
    void refreshList();
    void refreshWeeklyHistory();
  }, [refreshList, refreshWeeklyHistory]);

  useEffect(() => {
    if (!list || listError) return;
    const id = setInterval(() => void refreshList(false), 5000);
    return () => clearInterval(id);
  }, [list, listError, refreshList]);

  // Honor a deep-link from elsewhere (e.g., the WeeklyDriftCard on the dashboard).
  useEffect(() => {
    if (initialReport) {
      setSelected(initialReport);
      onConsumeInitial?.();
    }
  }, [initialReport, onConsumeInitial]);

  useEffect(() => {
    if (!selected) {
      setReport(null);
      setReportError(null);
      return;
    }
    let active = true;
    setReport(null);
    setReportLoading(true);
    setReportError(null);
    void api.driftReport(selected)
      .then((result) => {
        if (active) setReport(result);
      })
      .catch((error) => {
        if (active) {
          setReportError(
            error instanceof Error ? error.message : "Drift report request failed",
          );
        }
      })
      .finally(() => {
        if (active) setReportLoading(false);
      });
    return () => {
      active = false;
    };
  }, [selected, reportRetry]);

  const triggerRun = async () => {
    confirmedRunStart.current = false;
    setRunning(true);
    setRunError(null);
    setRunStatusUnknown(false);
    try {
      await api.driftRun(windowDays, limit);
      confirmedRunStart.current = true;
      setRunStatusUnknown(true);
      await refreshList(false);
    } catch (error) {
      const detail = error instanceof Error ? error.message : "Drift run failed";
      confirmedRunStart.current = false;
      setRunError(detail);
      setRunStatusUnknown(false);
      setRunning(false);
      toast("error", detail);
    }
  };

  const hasHistory = weeklyHistory.length > 0;

  return (
    <div className="space-y-5">
      <header className="flex items-start justify-between gap-4 border-b border-border pb-3">
        <div className="min-w-0">
          <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
            Self-audit
          </div>
          <h1 className="mt-1 text-base font-semibold tracking-tight text-fg">
            Drift Reports
          </h1>
          <p className="mt-1 max-w-xl text-[11px] leading-relaxed text-fg-muted">
            Re-run normal sessions in blind mode, measure how much precedent
            moved the verdict.
          </p>
        </div>
        <button onClick={() => void refreshList()} className="btn shrink-0 text-xs">
          <RotateCcw className="w-3.5 h-3.5" /> Refresh
        </button>
      </header>

      <div className="cg-command-panel flex flex-wrap items-center gap-x-5 gap-y-3">
        <div className="flex items-center gap-2">
          <span className="label">Window</span>
          <input
            type="number"
            min={1}
            max={90}
            value={windowDays}
            onChange={(e) => setWindowDays(Number(e.target.value))}
            className="input w-20 text-xs"
          />
          <span className="text-[10px] font-mono text-fg-dim">days</span>
        </div>
        <div className="flex items-center gap-2">
          <span className="label">Limit</span>
          <input
            type="number"
            min={1}
            max={50}
            value={limit}
            onChange={(e) => setLimit(Number(e.target.value))}
            className="input w-20 text-xs"
          />
          <span className="text-[10px] font-mono text-fg-dim">sessions</span>
        </div>
        <button
          onClick={triggerRun}
          disabled={running}
          className="btn btn-primary ml-auto text-xs"
        >
          {running ? (
            <><Loader2 className="w-3.5 h-3.5 animate-spin" /> Running…</>
          ) : (
            <><Play className="w-3.5 h-3.5" /> Run drift now</>
          )}
        </button>
      </div>

      {runError && (
        <div
          role="alert"
          className="flex flex-wrap items-center justify-between gap-3 rounded border border-danger/40 bg-danger/5 px-3.5 py-2.5"
        >
          <div>
            <div className="font-mono text-xs font-semibold text-danger">
              Drift run failed
            </div>
            <div className="mt-1 font-mono text-[10px] text-fg-dim">{runError}</div>
          </div>
          <button type="button" className="btn text-xs" onClick={() => void triggerRun()}>
            Retry run
          </button>
        </div>
      )}

      {runStatusUnknown && (
        <div
          role="alert"
          className="flex flex-wrap items-center justify-between gap-3 rounded border border-warning/40 bg-warning/5 px-3.5 py-2.5"
        >
          <div>
            <div className="font-mono text-xs font-semibold text-warning">
              Drift run accepted; status unknown
            </div>
            <div className="mt-1 font-mono text-[10px] text-fg-dim">
              The launch remains disabled until Council confirms the run has stopped.
            </div>
          </div>
          <button
            type="button"
            className="btn text-xs"
            onClick={() => void refreshList(false)}
          >
            Retry status
          </button>
        </div>
      )}

      {weeklyError && (
        <div
          role="alert"
          className="flex flex-wrap items-center justify-between gap-3 rounded border border-danger/40 bg-danger/5 px-3.5 py-2.5"
        >
          <div>
            <div className="font-mono text-xs font-semibold text-danger">
              Could not load weekly drift history
            </div>
            <div className="mt-1 font-mono text-[10px] text-fg-dim">{weeklyError}</div>
          </div>
          <button
            type="button"
            className="btn text-xs"
            onClick={() => void refreshWeeklyHistory()}
          >
            Retry history
          </button>
        </div>
      )}

      <div className="grid grid-cols-12 gap-5">
        <aside className="col-span-12 lg:col-span-3">
          <div className="cg-section-label">Reports</div>
          <div className="max-h-[70vh] overflow-y-auto rounded border border-border bg-bg-elevated">
            {listLoading && (
              <div className="p-3 text-[11px] font-mono leading-relaxed text-fg-dim">
                Loading drift reports…
              </div>
            )}
            {!listLoading && listError && (
              <div role="alert" className="space-y-3 p-3">
                <div className="text-[11px] font-mono font-semibold text-danger">
                  Could not load drift reports
                </div>
                <div className="text-[10px] font-mono text-fg-dim">{listError}</div>
                <button
                  type="button"
                  className="btn text-xs"
                  onClick={() => void refreshList()}
                >
                  Retry reports
                </button>
              </div>
            )}
            {!listLoading && !listError && (list?.reports ?? []).length === 0 && (
              <div className="p-3 text-[11px] font-mono leading-relaxed text-fg-dim">
                No reports yet. Run drift now to generate one.
              </div>
            )}
            {!listLoading && !listError && (list?.reports ?? []).map((r) => (
              <button
                key={r.name}
                onClick={() => setSelected(r.name)}
                className={cn(
                  "block w-full border-b border-l-2 border-l-transparent border-b-border px-3 py-2 text-left transition-colors last:border-b-0",
                  selected === r.name
                    ? "border-l-amber bg-amber/[0.06]"
                    : "hover:bg-bg-overlay",
                )}
              >
                <div className={cn(
                  "truncate text-[11px] font-mono",
                  selected === r.name ? "text-amber" : "text-fg-muted",
                )}>
                  {r.name}
                </div>
                <div className="mt-0.5 text-[10px] font-mono text-fg-dim tabular-nums">
                  {r.mtime.slice(0, 16).replace("T", " ")}
                </div>
              </button>
            ))}
          </div>
        </aside>

        {hasHistory && (
          <div className="col-span-12 lg:col-span-3">
            <div className="cg-section-label">Weekly History</div>
            <div className="max-h-60 overflow-y-auto rounded border border-border bg-bg-elevated">
              {weeklyHistory.map((w) => {
                const tone = driftTone(w.avg_drift);
                return (
                  <div
                    key={w.ts}
                    className="border-b border-border px-3 py-2 last:border-b-0"
                  >
                    <div className="flex items-center justify-between text-[11px] font-mono">
                      <span className="text-fg-muted tabular-nums">
                        {w.ts.slice(0, 10)}
                      </span>
                      <span className={cn(
                        "tabular-nums",
                        tone === "danger" && "text-danger",
                        tone === "warning" && "text-warning",
                        tone === "success" && "text-success",
                      )}>
                        {w.avg_drift.toFixed(3)}
                      </span>
                    </div>
                    <div className="mt-0.5 text-[10px] font-mono text-fg-dim tabular-nums">
                      {w.sessions_analyzed} sessions · {w.confidence_flips} flips
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        <section className={cn(
          "col-span-12 max-h-[80vh] overflow-y-auto rounded border border-border bg-bg-elevated p-6",
          hasHistory ? "lg:col-span-6" : "lg:col-span-9",
        )}>
          {reportLoading && (
            <div className="font-mono text-sm text-fg-dim">
              Loading drift report…
            </div>
          )}
          {!reportLoading && reportError && (
            <div role="alert" className="space-y-3">
              <div className="font-mono text-sm font-semibold text-danger">
                Could not load drift report
              </div>
              <div className="font-mono text-xs text-fg-dim">{reportError}</div>
              <button
                type="button"
                className="btn text-xs"
                onClick={() => setReportRetry((attempt) => attempt + 1)}
              >
                Retry report
              </button>
            </div>
          )}
          {!report && !reportLoading && !reportError && (
            <div className="font-mono text-sm text-fg-dim">
              Select or generate a drift report.
            </div>
          )}
          {report && (
            <motion.article
              key={report.name}
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              className="ruling max-w-none"
            >
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {report.content}
              </ReactMarkdown>
            </motion.article>
          )}
        </section>
      </div>
    </div>
  );
}
