"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeftRight,
  ChevronRight,
  FileDown,
  GitBranch,
  GitFork,
  Layers,
  ScrollText,
  Search,
  X,
} from "lucide-react";
import { api } from "@/lib/api";
import {
  buildThemeRows,
  clusterSessionIds,
  HISTORY_SESSION_LIST_LIMIT,
  selectedThemeLabels,
} from "@/lib/clusters";
import { downloadSessionPdf } from "@/lib/pdf-export";
import { ModeChip } from "./proceeding/ModeChips";
import { ProceedingMetrics } from "./proceeding/ProceedingMetrics";
import { ProceedingRecordHead } from "./proceeding/ProceedingRecordHead";
import { ProceedingPhaseRail } from "./proceeding/ProceedingPhaseRail";
import { ProceedingRulingColumn } from "./proceeding/ProceedingRulingColumn";
import { HistoryRoundLedger } from "./proceeding/SeatLedger";
import { buildPhasesForSession } from "@/lib/proceeding-phases";
import {
  cn,
} from "@/lib/cn";
import { proceedingTitle } from "@/lib/proceeding-display";
import type {
  ClustersResponse,
  LineageResponse,
  SessionDetail,
  SessionIndexEntry,
} from "@/lib/types";
import type { StartPayload } from "@/lib/ws";
import ForkModal from "./ForkModal";
import SynthesisDiff from "./SynthesisDiff";

export default function SessionExplorer({
  onLaunch,
  initialSelectedId,
  apiStatus = "online",
  apiError,
  onRetryConnection,
}: {
  onLaunch?: (start: StartPayload) => void;
  initialSelectedId?: string;
  apiStatus?: "loading" | "online" | "error";
  apiError?: string | null;
  onRetryConnection?: () => void;
}) {
  const [sessions, setSessions] = useState<SessionIndexEntry[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [listReload, setListReload] = useState(0);
  const [q, setQ] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailReload, setDetailReload] = useState(0);
  const [lineage, setLineage] = useState<LineageResponse | null>(null);
  const [forkFor, setForkFor] = useState<SessionIndexEntry | null>(null);
  const [diffPair, setDiffPair] = useState<{ a: string; b: string } | null>(null);
  const [compareMode, setCompareMode] = useState<string | null>(null);
  const [clusters, setClusters] = useState<ClustersResponse | null>(null);
  const [clusterFilter, setClusterFilter] = useState<Set<number>>(new Set());
  const [themesOpen, setThemesOpen] = useState(false);
  const appliedInitialId = useRef<string | null>(null);

  useEffect(() => {
    if (apiStatus === "error") {
      setLoading(false);
      setSessions([]);
      setLoadError(apiError ?? "Council bridge unreachable");
      setSelected(null);
      return;
    }
    if (apiStatus === "loading") {
      setLoading(true);
      return;
    }
    setLoading(true);
    setLoadError(null);
    let cancelled = false;
    api
      .sessions(HISTORY_SESSION_LIST_LIMIT)
      .then((r) => {
        if (cancelled) return;
        setSessions(r.sessions);
        setSelected((current) =>
          current && r.sessions.some((session) => session.id === current)
            ? current
            : null,
        );
        setLoading(false);
      })
      .catch((e) => {
        if (cancelled) return;
        setSessions([]);
        setSelected(null);
        setLoadError(errorMessage(e));
        setLoading(false);
      });
    api.clusters().then(setClusters).catch(() => setClusters(null));
    return () => {
      cancelled = true;
    };
  }, [apiStatus, apiError, listReload]);

  useEffect(() => {
    if (clusterFilter.size > 0) setThemesOpen(true);
  }, [clusterFilter]);

  useEffect(() => {
    if (!initialSelectedId || initialSelectedId === appliedInitialId.current) return;
    appliedInitialId.current = initialSelectedId;
    setSelected(initialSelectedId);
  }, [initialSelectedId]);

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      setDetailError(null);
      setDetailLoading(false);
      setLineage(null);
      return;
    }
    let cancelled = false;
    setDetail(null);
    setDetailError(null);
    setDetailLoading(true);
    setLineage(null);
    api
      .session(selected)
      .then((d) => {
        if (!cancelled) setDetail(d as SessionDetail);
      })
      .catch((e) => {
        if (!cancelled) setDetailError(errorMessage(e));
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });
    api
      .lineage(selected)
      .then((value) => {
        if (!cancelled) setLineage(value);
      })
      .catch(() => {
        if (!cancelled) setLineage(null);
      });
    return () => {
      cancelled = true;
    };
  }, [selected, detailReload]);

  const retryList = () => {
    setLoadError(null);
    setLoading(true);
    if (apiStatus === "error") onRetryConnection?.();
    setListReload((attempt) => attempt + 1);
  };

  const retryDetail = () => {
    setDetailError(null);
    setDetailLoading(true);
    setDetailReload((attempt) => attempt + 1);
  };

  const filtered = useMemo(() => {
    const ids =
      clusterFilter.size && clusters
        ? clusterSessionIds(clusters.clusters, clusterFilter)
        : null;
    const ql = q.trim().toLowerCase();
    return sessions.filter((s) => {
      if (ids && !ids.has(s.id)) return false;
      if (!ql) return true;
      return (
        s.topic.toLowerCase().includes(ql) ||
        s.keywords?.some((k) => k.toLowerCase().includes(ql)) ||
        s.cabinet.toLowerCase().includes(ql) ||
        s.id.toLowerCase().includes(ql)
      );
    });
  }, [sessions, q, clusters, clusterFilter]);

  const activeThemes = useMemo(
    () =>
      clusterFilter.size && clusters
        ? selectedThemeLabels(clusters.clusters, clusterFilter, sessions)
        : [],
    [clusters, clusterFilter, sessions],
  );

  const toggleCluster = (id: number) =>
    setClusterFilter((prev) => {
      // Single-theme filter: one active theme at a time; click again to clear.
      if (prev.has(id) && prev.size === 1) return new Set();
      return new Set([id]);
    });

  const selectedEntry = sessions.find((s) => s.id === selected);

  if (diffPair) {
    return (
      <SynthesisDiff
        parentId={diffPair.a}
        childId={diffPair.b}
        onClose={() => setDiffPair(null)}
      />
    );
  }

  return (
    <div className="cg-history-workspace">
      <aside className="cg-rail">
        <div className="shrink-0 px-3.5 py-2 border-b border-border">
          <div className="relative">
            <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-fg-dim pointer-events-none" />
            <input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Search topics, cabinet, session id…"
              aria-label="Search proceedings"
              className="input pl-8 h-8 text-[11px]"
            />
          </div>
        </div>
        {clusters && clusters.clusters.length > 0 && (
          <ThemesDisclosure
            clusters={clusters}
            sessions={sessions}
            selected={clusterFilter}
            open={themesOpen}
            onOpenChange={setThemesOpen}
            activeThemeLabel={activeThemes[0] ?? null}
            onToggle={toggleCluster}
            onClear={() => setClusterFilter(new Set())}
          />
        )}
        {activeThemes.length > 0 && (
          <div className="shrink-0 px-3 py-1.5 border-b border-amber/30 bg-amber/[0.07] text-[10px] font-mono text-amber font-semibold leading-snug">
            <span className="tabular-nums">{filtered.length}</span> proceedings ·{" "}
            {activeThemes.join(" + ")}
          </div>
        )}
        <div className="shrink-0 px-2 pt-1.5 pb-1 text-[10px] font-mono text-fg-dim">
          {loading
            ? "Loading proceedings…"
            : activeThemes.length > 0
              ? `${filtered.length} matching · ${sessions.length} loaded`
              : clusters && clusters.n_sessions > sessions.length
                ? `${filtered.length} shown · latest ${sessions.length} of ${clusters.n_sessions} indexed`
                : `${filtered.length} of ${sessions.length} proceedings`}
          {compareMode && (
            <span className="ml-1 text-amber">
              (click session to diff vs {compareMode.slice(0, 8)})
            </span>
          )}
        </div>
        <div className="cg-rail-sessions" data-testid="session-list">
          {loading && (
            <p className="px-2 py-4 text-[11px] font-mono text-fg-dim animate-pulse">
              Indexing proceedings…
            </p>
          )}
          {!loading && loadError && (
            <div
              data-testid="history-list-error"
              className="mx-1.5 my-2 p-3 border border-danger/40 rounded bg-danger/5 text-[11px] font-mono leading-relaxed"
            >
              <p className="text-danger font-semibold mb-1">Proceedings unavailable</p>
              <p className="text-fg-muted mb-2">{loadError}</p>
              <p className="text-fg-dim mb-2">
                Start the council sidecar, then reload:
              </p>
              <code className="block text-[10px] text-fg-muted bg-bg-overlay p-2 rounded border border-border">
                make warroom-browser
              </code>
              <button
                type="button"
                data-testid="history-list-retry"
                className="btn btn-primary mt-2 w-full"
                onClick={retryList}
              >
                Retry proceedings
              </button>
            </div>
          )}
          {!loading && !loadError && sessions.length === 0 && (
            <p
              data-testid="history-empty"
              className="px-2 py-4 text-[11px] font-mono text-fg-dim leading-relaxed"
            >
              No sessions in the index yet. Run a deliberation from Deliberate —
              filings appear here after <code className="text-fg-muted">--reindex</code> or
              a completed session save.
            </p>
          )}
          {!loading &&
            !loadError &&
            clusterFilter.size > 0 &&
            filtered.length === 0 && (
              <p className="px-2 py-4 text-[11px] font-mono text-fg-dim leading-relaxed border border-border rounded mx-1.5 bg-bg-overlay/40">
                No proceedings from this theme appear in the latest{" "}
                {sessions.length} loaded sessions. Clear the theme filter or search
                by topic.
              </p>
            )}
          {!loading &&
            !loadError &&
            filtered.length === 0 &&
            clusterFilter.size === 0 &&
            sessions.length > 0 &&
            q.trim() && (
              <p className="px-2 py-3 text-[11px] font-mono text-fg-dim">
                No proceedings match your search.
              </p>
            )}
          {!loading &&
            filtered.map((s) => (
            <button
              key={s.id}
              type="button"
              onClick={() => {
                if (compareMode && compareMode !== s.id) {
                  setDiffPair({ a: compareMode, b: s.id });
                  setCompareMode(null);
                } else {
                  setSelected(s.id);
                }
              }}
              className={cn(
                "cg-session-row",
                selected === s.id && "selected",
                compareMode === s.id && "border-l-cyan",
              )}
            >
              <div>
                <div className="text-[10px] font-mono text-fg-dim mb-0.5">
                  <span className="text-fg-muted">{s.id.slice(0, 12)}</span>
                  {" · "}
                  {s.ts.slice(0, 10)}
                  {" · "}
                  <ModeChip mode={s.mode} />
                </div>
                <div
                  className="text-[11px] leading-snug text-fg-muted line-clamp-2"
                  title={s.topic}
                >
                  {proceedingTitle(s.topic, 96)}
                </div>
              </div>
              <div
                className={cn(
                  "text-[10px] font-semibold font-mono pt-0.5",
                  selected === s.id ? "text-success" : "text-fg-dim",
                )}
              >
                {Math.round((s.convergence ?? 0) * 100)}%
              </div>
            </button>
            ))}
        </div>
      </aside>

      {!detail && (
        <section className="cg-record-empty">
          {loadError && (
            <div className="flex flex-col items-center justify-center p-8 text-center max-w-md mx-auto min-h-[420px]">
              <p className="font-authority text-lg text-fg mb-2">Proceeding record</p>
              <p className="text-sm text-fg-muted font-mono leading-relaxed">
                The ledger shell is ready. Connect the council bridge at{" "}
                <span className="text-amber">127.0.0.1:8765</span> to load sessions,
                seat rows, validation, and rulings.
              </p>
            </div>
          )}
          {!loadError && !loading && selected && detailLoading && (
            <div
              data-testid="history-detail-loading"
              className="flex items-center justify-center p-8 text-fg-dim font-mono text-sm animate-pulse min-h-[420px]"
            >
              Loading selected proceeding…
            </div>
          )}
          {!loadError && !loading && selected && detailError && (
            <div
              data-testid="history-detail-error"
              className="flex flex-col items-center justify-center p-8 text-center max-w-md mx-auto min-h-[420px]"
            >
              <p className="font-authority text-lg text-danger mb-2">
                Proceeding record unavailable
              </p>
              <p className="text-sm text-fg-muted font-mono leading-relaxed">
                {detailError}
              </p>
              <button
                type="button"
                data-testid="history-detail-retry"
                onClick={retryDetail}
                className="btn btn-primary mt-4"
              >
                Retry record
              </button>
            </div>
          )}
          {!loadError && !loading && sessions.length > 0 && !selected && (
            <div className="flex items-center justify-center p-8 text-fg-dim font-mono text-sm min-h-[420px]">
              Select a proceeding from the list on the left.
            </div>
          )}
          {!loadError && !loading && sessions.length === 0 && (
            <div className="flex items-center justify-center p-8 text-fg-dim font-mono text-sm text-center max-w-sm mx-auto min-h-[420px]">
              No proceedings on record. Convene a council session to populate the docket.
            </div>
          )}
          {loading && !selected && (
            <div className="flex items-center justify-center p-8 text-fg-dim font-mono text-sm animate-pulse min-h-[420px]">
              Loading proceeding record…
            </div>
          )}
        </section>
      )}
      {detail && selectedEntry && (
        <SessionDetailView
          detail={detail}
          entry={selectedEntry}
          lineage={lineage}
          onFork={() => setForkFor(selectedEntry)}
          onToggleCompare={() =>
            setCompareMode(compareMode === selected ? null : selected)
          }
          compareActive={compareMode === selected}
          canFork={Boolean(onLaunch)}
        />
      )}

      {forkFor && onLaunch && (
        <ForkModal
          parent={forkFor}
          onClose={() => setForkFor(null)}
          onLaunch={onLaunch}
        />
      )}
    </div>
  );
}

function SessionDetailView({
  detail,
  entry,
  lineage,
  onFork,
  onToggleCompare,
  compareActive,
  canFork,
}: {
  detail: SessionDetail;
  entry: SessionIndexEntry;
  lineage: LineageResponse | null;
  onFork: () => void;
  onToggleCompare: () => void;
  compareActive: boolean;
  canFork: boolean;
}) {
  const [synthesisOnly, setSynthesisOnly] = useState(false);
  const [exporting, setExporting] = useState(false);
  const phases = useMemo(() => buildPhasesForSession(detail), [detail]);

  const finalConv = detail.rounds.length
    ? detail.rounds[detail.rounds.length - 1].convergence_score ?? 0
    : entry.convergence ?? 0;
  const exportPdf = async () => {
    setExporting(true);
    try {
      await downloadSessionPdf(detail.session_id);
    } finally {
      setExporting(false);
    }
  };

  return (
    <>
      <div className="cg-record-primary">
        <ProceedingRecordHead
          key={detail.session_id}
          mode={detail.mode}
          cabinetLabel={detail.cabinet_name}
          topic={detail.topic}
          sessionId={detail.session_id}
          executionRoute={detail.execution_route ?? entry.execution_route}
          gatewaySensitivity={detail.gateway_sensitivity ?? entry.gateway_sensitivity}
          actions={
            <>
              <button
                type="button"
                onClick={onFork}
                className="btn btn-primary text-[10px]"
                disabled={!canFork}
              >
                <GitFork className="w-3.5 h-3.5" />
                Fork
              </button>
              <button
                type="button"
                onClick={onToggleCompare}
                className={cn("btn text-[10px]", compareActive && "btn-primary")}
              >
                <ArrowLeftRight className="w-3.5 h-3.5" />
                {compareActive ? "Cancel diff" : "Diff"}
              </button>
              <button
                type="button"
                data-testid="session-export-pdf"
                onClick={() => void exportPdf()}
                disabled={exporting}
                className="btn text-[10px]"
              >
                <FileDown className="w-3.5 h-3.5" />
                {exporting ? "…" : "Export"}
              </button>
              <button
                type="button"
                data-testid="synthesis-only-toggle"
                onClick={() => setSynthesisOnly((v) => !v)}
                title={
                  synthesisOnly
                    ? "Show full round-by-round record"
                    : "Focus on Council ruling only — best for long transcripts"
                }
                className={cn(
                  "btn text-[10px]",
                  synthesisOnly ? "btn-primary" : "border-amber/40 text-amber",
                )}
              >
                {synthesisOnly ? (
                  <Layers className="w-3.5 h-3.5" />
                ) : (
                  <ScrollText className="w-3.5 h-3.5" />
                )}
                {synthesisOnly ? "Full record" : "Ruling only"}
              </button>
            </>
          }
        >
          {lineage && (lineage.parent || lineage.children.length > 0) && (
            <div className="flex flex-wrap items-center gap-2 mt-2 text-[10px] font-mono text-fg-dim">
              <GitBranch className="w-3 h-3 text-amber shrink-0" />
              {lineage.parent && (
                <span>
                  forked from{" "}
                  <span className="text-fg-muted">{lineage.parent.parent_id}</span>
                </span>
              )}
              {lineage.children.length > 0 && (
                <span>
                  {lineage.children.length} child fork
                  {lineage.children.length === 1 ? "" : "s"}
                </span>
              )}
            </div>
          )}
        </ProceedingRecordHead>

        <ProceedingMetrics
          rounds={detail.rounds.length}
          tokens={detail.total_tokens}
          costUsd={detail.total_cost_usd}
          latencyMs={detail.total_latency_ms}
          convergence={finalConv}
        />

        {!synthesisOnly && (
          <>
            <ProceedingPhaseRail phases={phases} />

            {detail.rounds.map((r) => (
              <HistoryRoundLedger key={r.round_num} round={r} />
            ))}
          </>
        )}
      </div>

      <ProceedingRulingColumn
        synthesis={detail.synthesis}
        synthesisModel={detail.synthesis_model}
        sessionId={detail.session_id}
        confidence={entry.confidence}
        placeholder="No synthesis recorded."
      />
    </>
  );
}

function ThemesDisclosure({
  clusters,
  sessions,
  selected,
  open,
  onOpenChange,
  activeThemeLabel,
  onToggle,
  onClear,
}: {
  clusters: ClustersResponse;
  sessions: SessionIndexEntry[];
  selected: ReadonlySet<number>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  activeThemeLabel: string | null;
  onToggle: (id: number) => void;
  onClear: () => void;
}) {
  const themeCount = clusters.clusters.length;

  return (
    <div className="cg-themes-disclosure" data-testid="cluster-tile">
      <button
        type="button"
        className="cg-themes-disclosure-toggle"
        aria-expanded={open}
        onClick={() => onOpenChange(!open)}
      >
        <ChevronRight
          className={cn(
            "w-3.5 h-3.5 shrink-0 text-fg-dim transition-transform duration-150",
            open && "rotate-90",
          )}
        />
        <span className="font-semibold uppercase tracking-widest text-fg-muted">Themes</span>
        <span className="text-fg-dim tabular-nums">({themeCount})</span>
        {activeThemeLabel && !open && (
          <span className="ml-auto truncate max-w-[45%] text-amber font-medium normal-case tracking-normal">
            {activeThemeLabel}
          </span>
        )}
      </button>
      {open && (
        <ThemesPanel
          clusters={clusters}
          sessions={sessions}
          selected={selected}
          onToggle={onToggle}
          onClear={onClear}
        />
      )}
    </div>
  );
}

function ThemesPanel({
  clusters,
  sessions,
  selected,
  onToggle,
  onClear,
}: {
  clusters: ClustersResponse;
  sessions: SessionIndexEntry[];
  selected: ReadonlySet<number>;
  onToggle: (id: number) => void;
  onClear: () => void;
}) {
  const rows = useMemo(
    () => buildThemeRows(clusters.clusters, sessions),
    [clusters.clusters, sessions],
  );
  const listNote =
    clusters.n_sessions > sessions.length
      ? `Latest ${sessions.length} of ${clusters.n_sessions} indexed proceedings loaded.`
      : null;

  return (
    <div className="cg-themes-panel">
      <p className="text-[9px] font-mono text-fg-dim mb-1.5 leading-relaxed">
        Grouped by similarity — filter proceedings below.
      </p>
      <div
        className="grid grid-cols-[1fr_auto] gap-2 px-1 pb-1 text-[9px] font-mono font-semibold uppercase tracking-widest text-fg-dim"
        title={
          listNote ??
          "Proceedings in the loaded list that match this theme when selected"
        }
      >
        <span>Keywords</span>
        <span>In list</span>
      </div>
      {selected.size > 0 && (
        <button
          type="button"
          data-testid="cluster-clear"
          onClick={onClear}
          className="mb-1.5 flex items-center gap-1 text-[10px] font-mono text-amber hover:text-amber/80"
        >
          <X className="w-3 h-3" />
          Clear theme filter
        </button>
      )}
      <div className="space-y-0">
        {rows.map((row) => {
          const on = selected.has(row.cluster.id);
          const inert = row.filterable === 0;
          return (
            <button
              key={row.cluster.id}
              type="button"
              data-testid="cluster-chip"
              aria-pressed={on}
              disabled={inert}
              onClick={() => !inert && onToggle(row.cluster.id)}
              title={
                inert
                  ? `${row.label}\nNo members in the loaded proceedings window.\n${row.countTitle}`
                  : row.sample
                    ? `${row.label}\n${row.countTitle}\nExample: ${row.sample}`
                    : `${row.label}\n${row.countTitle}`
              }
              className={cn(
                "cg-theme-entry group w-full text-left rounded-sm transition-colors",
                on && "selected",
                inert && "inert",
              )}
            >
              <div className="min-w-0">
                <div
                  className={cn(
                    "text-[11px] font-mono leading-snug",
                    on ? "text-fg font-medium" : "text-fg-muted",
                  )}
                >
                  {row.label}
                </div>
                {inert && (
                  <div className="text-[9px] font-mono text-fg-dim mt-0.5">
                    none in loaded window
                  </div>
                )}
                {row.sample && !inert && (
                  <div className="cg-theme-example">e.g. &quot;{row.sample}&quot;</div>
                )}
              </div>
              <span
                className={cn(
                  "cg-theme-count",
                  (row.filterable > 0 || on) && "active",
                )}
                title={row.countTitle}
              >
                {row.countText}
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}

function errorMessage(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}
