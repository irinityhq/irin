"use client";

import { useState } from "react";
import { CheckCircle2, Loader2, Radar, RefreshCw, XCircle } from "lucide-react";
import { cn } from "@/lib/cn";
import { providerColor } from "@/lib/cn";
import { useDiscover } from "@/lib/use-discover";

/**
 * Provider discovery (feature contract) — UI mirror of `./council --discover` via
 * `GET /api/discover`. Missing providers show which env var NAME to set;
 * the backend never sends key values or fragments.
 */

/** Left-bar tint for a provider row — mirrors the seat-ledger treatment. */
function providerBorder(name: string): string {
  switch (providerColor(name)) {
    case "amber":
      return "border-l-amber";
    case "cyan":
      return "border-l-cyan";
    case "success":
      return "border-l-success";
    case "magenta":
      return "border-l-magenta";
    default:
      return "border-l-border-bright";
  }
}

export default function DiscoverPanel() {
  const { data, loading, error, rescan } = useDiscover();
  const [showLog, setShowLog] = useState(false);

  const total = data?.providers.length ?? 0;
  const availableCount = data?.providers.filter((p) => p.available).length ?? 0;

  return (
    <div className="max-w-4xl mx-auto space-y-5" data-testid="discover-view">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="label flex items-center gap-1.5 text-fg-dim">
            <Radar className="w-3 h-3 text-fg-dim" />
            Provider Discovery
          </div>
          <p className="mt-1.5 max-w-xl text-xs leading-relaxed text-fg-muted">
            Same scan as{" "}
            <code className="font-mono text-cyan">./council --discover</code> —
            configured env keys, supported local CLI binaries and adapters, Vertex ADC,
            and localhost runtimes. Detected means present, not authenticated or reachable.
          </p>
        </div>
        <button
          type="button"
          data-testid="discover-refresh"
          onClick={() => void rescan()}
          disabled={loading}
          className="btn text-xs"
        >
          {loading ? (
            <Loader2 className="w-3.5 h-3.5 animate-spin" />
          ) : (
            <RefreshCw className="w-3.5 h-3.5" />
          )}
          Rescan
        </button>
      </div>

      {loading ? (
        <div className="space-y-2 border border-border bg-bg-elevated p-4">
          {Array.from({ length: 4 }).map((_, i) => (
            <div key={i} className="h-8 animate-pulse bg-border/40 rounded" />
          ))}
          <div className="text-center font-mono text-xs text-fg-dim pt-2">Scanning providers…</div>
        </div>
      ) : error ? (
        <div className="border border-border border-l-2 border-l-danger bg-bg-deep p-4 font-mono text-xs text-danger">
          Discovery failed: {error}
        </div>
      ) : data && data.providers.length === 0 ? (
        <div className="border border-border bg-bg-elevated px-4 py-10 text-center font-mono text-xs text-fg-muted">
          No providers reported. Check council --serve logs.
        </div>
      ) : data ? (
        <>
          <div className="flex items-baseline gap-3 border-y border-border py-3">
            <span className="text-3xl font-semibold leading-none text-amber tabular-nums">
              {availableCount}
            </span>
            <div className="flex flex-col leading-tight">
              <span className="text-[9px] font-mono uppercase tracking-widest text-fg-dim">
                Detected
              </span>
              <span className="font-mono text-xs text-fg-dim tabular-nums">
                of {total} known provider paths
              </span>
            </div>
          </div>

          <div className="space-y-1.5">
            {data.providers.map((p) => (
              <div
                key={p.name}
                data-testid="discover-provider"
                className={cn(
                  "flex flex-col gap-2 border border-border border-l-2 bg-bg-elevated px-4 py-3 md:flex-row md:items-center md:gap-4",
                  p.available ? providerBorder(p.name) : "border-l-border opacity-70",
                )}
              >
                <div className="flex items-center gap-2 shrink-0 md:w-64">
                  {p.available ? (
                    <CheckCircle2 className="w-3.5 h-3.5 shrink-0 text-success" />
                  ) : (
                    <XCircle className="w-3.5 h-3.5 shrink-0 text-fg-dim" />
                  )}
                  <div className="min-w-0">
                    <div className="truncate font-mono text-sm font-medium text-fg">
                      {p.label}
                    </div>
                    {p.label !== p.name && (
                      <div className="truncate font-mono text-[9px] text-fg-dim">
                        {p.name}
                      </div>
                    )}
                  </div>
                </div>
                <div className="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
                  {p.family && (
                    <span className="chip text-[10px]">family: {p.family}</span>
                  )}
                  {p.transport && (
                    <span className="chip chip-cyan text-[10px]">transport: {p.transport}</span>
                  )}
                  {p.available && p.gateway_supported === false && (
                    <span className="chip text-[10px] text-amber">Direct only</span>
                  )}
                  {p.source && (
                    <span className="chip text-[10px]">source: {p.source}</span>
                  )}
                  {p.models.map((m) => (
                    <span key={m} className="chip font-mono text-[10px]">
                      {m}
                    </span>
                  ))}
                  {p.models.length === 0 && p.available && (
                    <span className="font-mono text-[10px] text-fg-dim">
                      models resolved at request time
                    </span>
                  )}
                </div>
                {!p.available && (
                  <div className="shrink-0 font-mono text-[10px] text-amber">
                    {p.env_hint ? (
                      <>set <code className="text-cyan">{p.env_hint}</code></>
                    ) : (
                      <>unavailable</>
                    )}
                  </div>
                )}
              </div>
            ))}
          </div>

          <div className="border border-border bg-bg-deep p-3">
            <button
              type="button"
              onClick={() => setShowLog((v) => !v)}
              className="font-mono text-[10px] uppercase tracking-widest text-fg-dim transition-colors hover:text-amber"
            >
              {showLog ? "▼" : "▶"} Discovery log ({data.log.length} lines)
            </button>
            {showLog && (
              <pre
                data-testid="discover-log"
                className="mt-2 max-h-64 overflow-y-auto whitespace-pre-wrap font-mono text-[10px] text-fg-muted"
              >
                {data.log.length ? data.log.join("\n") : "(empty)"}
              </pre>
            )}
          </div>
        </>
      ) : null}
    </div>
  );
}
