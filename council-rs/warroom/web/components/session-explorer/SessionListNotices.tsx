import type { SessionIndexEntry } from "@/lib/types";

export function SessionListNotices({
  loading,
  loadError,
  sessions,
  clusterFilter,
  filtered,
  q,
  onRetry,
}: {
  loading: boolean;
  loadError: string | null;
  sessions: SessionIndexEntry[];
  clusterFilter: ReadonlySet<number>;
  filtered: SessionIndexEntry[];
  q: string;
  onRetry: () => void;
}) {
  return (
    <>
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
            onClick={onRetry}
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
    </>
  );
}
