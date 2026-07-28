"use client";

import { useEffect, useRef, useState } from "react";
import { Search } from "lucide-react";
import type { SessionIndexEntry } from "@/lib/types";
import type { StartPayload } from "@/lib/ws";
import ForkModal from "./ForkModal";
import SynthesisDiff from "./SynthesisDiff";
import { RecordEmptyState } from "./session-explorer/RecordEmptyState";
import { SessionDetailView } from "./session-explorer/SessionDetailView";
import { SessionListNotices } from "./session-explorer/SessionListNotices";
import { SessionRow } from "./session-explorer/SessionRow";
import { ThemesDisclosure } from "./session-explorer/ThemesDisclosure";
import { useSessionDetail } from "./session-explorer/useSessionDetail";
import { useSessionList } from "./session-explorer/useSessionList";
import { useThemeFilter } from "./session-explorer/useThemeFilter";

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
  const [selected, setSelected] = useState<string | null>(null);
  const [forkFor, setForkFor] = useState<SessionIndexEntry | null>(null);
  const [diffPair, setDiffPair] = useState<{ a: string; b: string } | null>(null);
  const [compareMode, setCompareMode] = useState<string | null>(null);
  const appliedInitialId = useRef<string | null>(null);

  const { sessions, loadError, loading, clusters, retryList } = useSessionList({
    apiStatus,
    apiError,
    onRetryConnection,
    setSelected,
  });
  const { detail, detailError, detailLoading, lineage, retryDetail } =
    useSessionDetail(selected);
  const {
    q,
    setQ,
    clusterFilter,
    themesOpen,
    setThemesOpen,
    filtered,
    activeThemes,
    toggleCluster,
    clearClusterFilter,
  } = useThemeFilter(sessions, clusters);

  useEffect(() => {
    if (!initialSelectedId || initialSelectedId === appliedInitialId.current) return;
    appliedInitialId.current = initialSelectedId;
    setSelected(initialSelectedId);
  }, [initialSelectedId]);

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
            onClear={clearClusterFilter}
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
          <SessionListNotices
            loading={loading}
            loadError={loadError}
            sessions={sessions}
            clusterFilter={clusterFilter}
            filtered={filtered}
            q={q}
            onRetry={retryList}
          />
          {!loading &&
            filtered.map((s) => (
            <SessionRow
              key={s.id}
              entry={s}
              selected={selected === s.id}
              compareMode={compareMode}
              onClick={() => {
                if (compareMode && compareMode !== s.id) {
                  setDiffPair({ a: compareMode, b: s.id });
                  setCompareMode(null);
                } else {
                  setSelected(s.id);
                }
              }}
            />
            ))}
        </div>
      </aside>

      {!detail && (
        <RecordEmptyState
          loadError={loadError}
          loading={loading}
          selected={selected}
          detailLoading={detailLoading}
          detailError={detailError}
          sessions={sessions}
          onRetryDetail={retryDetail}
        />
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
