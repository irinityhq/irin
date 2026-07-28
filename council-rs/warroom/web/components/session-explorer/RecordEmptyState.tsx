import type { SessionIndexEntry } from "@/lib/types";

export function RecordEmptyState({
  loadError,
  loading,
  selected,
  detailLoading,
  detailError,
  sessions,
  onRetryDetail,
}: {
  loadError: string | null;
  loading: boolean;
  selected: string | null;
  detailLoading: boolean;
  detailError: string | null;
  sessions: SessionIndexEntry[];
  onRetryDetail: () => void;
}) {
  return (
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
            onClick={onRetryDetail}
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
  );
}
