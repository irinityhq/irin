"use client";

import { useCallback, useEffect, useState } from "react";
import { CheckCircle2, ChevronDown, ChevronRight, Clock, Copy, ShieldCheck, ShieldX } from "lucide-react";
import { api } from "@/lib/api";
import { cn, fmtCost } from "@/lib/cn";
import {
  parseOutboxList,
  type OutboxDetailResponse,
  type OutboxSummary,
  type WorkerProvenance,
} from "@/lib/governance";

export default function OutboxView(
  { initialTenant: _initialTenant }: { initialTenant?: string } = {},
) {
  const [tenant, setTenant] = useState<string | null>(null);
  const [directives, setDirectives] = useState<OutboxSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<OutboxDetailResponse | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const data = parseOutboxList(await api.governanceOutbox());
      setTenant(data.canary_tenant);
      setDirectives(data.directives);
      setError(null);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    const onConfig = () => void load();
    window.addEventListener("warroom-config-changed", onConfig);
    return () => window.removeEventListener("warroom-config-changed", onConfig);
  }, [load]);

  const select = useCallback(async (id: string) => {
    if (selectedId === id) {
      setSelectedId(null);
      setDetail(null);
      return;
    }
    setSelectedId(id);
    setDetail(null);
    setDetailLoading(true);
    try {
      setDetail(await api.governanceOutboxDetail(id));
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setDetailLoading(false);
    }
  }, [selectedId]);

  return (
    <div data-testid="outbox-view" className="space-y-5">
      <div className="flex items-end justify-between border-b border-border pb-3">
        <div>
          <h2 className="text-[11px] font-mono font-semibold uppercase tracking-widest text-fg-muted">
            Gateway Outbox Provenance
          </h2>
          <p className="text-sm text-fg-muted mt-1">
            Authenticated signed directives with exact-byte Ed25519 verification on detail.
          </p>
        </div>
        {tenant && (
          <div className="text-right">
            <div className="label">Configured canary</div>
            <div className="text-xs font-mono text-fg-bright">{tenant}</div>
          </div>
        )}
      </div>

      {loading ? (
        <div className="panel p-12 text-center text-xs font-mono text-fg-dim animate-pulse">
          Fetching authenticated Gateway outbox…
        </div>
      ) : error && directives.length === 0 ? (
        <UnavailablePanel detail={error} />
      ) : directives.length === 0 ? (
        <div className="panel p-12 text-center text-sm text-fg-muted">
          No directives found in the configured canary outbox.
        </div>
      ) : (
        <>
          {error && <div className="panel p-3 text-xs font-mono text-amber">{error}</div>}
          <div className="border border-border rounded divide-y divide-border overflow-hidden bg-bg-elevated">
            {directives.map((directive) => (
              <OutboxRow
                key={directive.id}
                record={directive}
                expanded={selectedId === directive.id}
                detail={selectedId === directive.id ? detail : null}
                detailLoading={selectedId === directive.id && detailLoading}
                onSelect={() => void select(directive.id)}
              />
            ))}
          </div>
        </>
      )}
    </div>
  );
}

function UnavailablePanel({ detail }: { detail: string }) {
  return (
    <div className="panel p-6 space-y-2">
      <div className="flex items-center gap-2 text-[11px] font-mono font-semibold uppercase tracking-widest text-fg-muted">
        <ShieldCheck className="w-4 h-4 text-amber" /> Gateway Outbox unavailable
      </div>
      <p className="text-sm text-fg">
        Council could not read the admin-authenticated Gateway outbox.
      </p>
      <p className="text-xs font-mono text-fg-dim">{detail}</p>
    </div>
  );
}

function OutboxRow({
  record,
  expanded,
  detail,
  detailLoading,
  onSelect,
}: {
  record: OutboxSummary;
  expanded: boolean;
  detail: OutboxDetailResponse | null;
  detailLoading: boolean;
  onSelect: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const [nowMs] = useState(() => Date.now());
  const isAcked = record.acked_at_ms != null;
  const isExpired = !isAcked && record.expires_at_ms != null && record.expires_at_ms < nowMs;

  const copySig = () => {
    void navigator.clipboard.writeText(record.signature.value);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  return (
    <div className="px-4 py-3 space-y-3">
      <button
        type="button"
        onClick={onSelect}
        className="w-full grid grid-cols-[auto_auto_minmax(0,1fr)_auto] items-center gap-3 text-left"
        aria-expanded={expanded}
      >
        {expanded ? <ChevronDown className="w-4 h-4 text-fg-dim" /> : <ChevronRight className="w-4 h-4 text-fg-dim" />}
        {isAcked ? (
          <CheckCircle2 className="w-4 h-4 text-success" />
        ) : isExpired ? (
          <Clock className="w-4 h-4 text-danger" />
        ) : (
          <Clock className="w-4 h-4 text-amber" />
        )}
        <div className="min-w-0">
          <div className="font-mono text-xs text-fg-bright font-semibold truncate">{record.id}</div>
          <div className="mt-0.5 flex items-center gap-2 text-[10px] font-mono text-fg-dim">
            <span className="chip">{record.verdict}</span>
            <span>{new Date(record.created_at_ms).toLocaleString()}</span>
          </div>
        </div>
        <span className={cn("chip", isAcked ? "chip-success" : isExpired ? "chip-danger" : "chip-amber")}>
          {record.status}
        </span>
      </button>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-x-6 gap-y-4 pl-11">
        <div className="space-y-4">
          <div>
            <div className="label mb-1">Council Session Correlation</div>
            {record.council_session_id ? (
              <div className="flex flex-col gap-1">
                <div className="text-xs font-mono text-fg break-all">{record.council_session_id}</div>
                <div className="text-[10px] font-mono text-fg-muted">
                  Cost: {record.council_cost_usd != null ? fmtCost(record.council_cost_usd) : "Not reported"}
                </div>
              </div>
            ) : (
              <div className="text-xs font-mono text-fg-muted">Not reported</div>
            )}
          </div>
          <WorkerProvenanceView provenance={record.worker_provenance} />
        </div>

        <div>
          <div className="label mb-1 flex items-center gap-1">
            <ShieldCheck className="w-3 h-3 text-fg-muted" /> Cryptographic Signature
          </div>
          <div className="bg-bg-deep rounded border border-border p-3 group relative">
            <div className="text-xs font-mono text-fg-muted break-all pr-8">{record.signature.value}</div>
            <button
              type="button"
              onClick={copySig}
              title={copied ? "Copied" : "Copy signature"}
              className="absolute top-2 right-2 p-1.5 rounded bg-bg-elevated hover:border-border-bright border border-border"
            >
              <Copy className="w-3 h-3 text-fg-dim" />
            </button>
          </div>
          <div className="text-[10px] font-mono text-fg-dim mt-2 flex gap-3">
            <span>Alg: {record.signature.alg}</span><span>Kid: {record.signature.kid}</span>
          </div>
        </div>
      </div>

      {expanded && (
        <div className="ml-11 panel p-4 space-y-3">
          {detailLoading ? (
            <div className="text-xs font-mono text-fg-dim animate-pulse">Verifying exact canonical bytes…</div>
          ) : detail ? (
            <>
              <div className="flex items-center gap-2">
                {detail.verification.verified ? (
                  <ShieldCheck className="w-4 h-4 text-success" />
                ) : (
                  <ShieldX className="w-4 h-4 text-danger" />
                )}
                <span className={cn("text-xs font-mono font-semibold", detail.verification.verified ? "text-success" : "text-danger")}>
                  {detail.verification.verified ? "Ed25519 verified over exact canonical UTF-8" : "Signature verification failed"}
                </span>
                <span className="text-[10px] font-mono text-fg-dim">{detail.verification.detail}</span>
              </div>
              <div>
                <div className="label mb-1">Signed canonical envelope</div>
                <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded border border-border bg-bg-deep p-3 text-[10px] font-mono text-fg-muted">
                  {detail.directive.envelope_json_canonical}
                </pre>
              </div>
            </>
          ) : null}
        </div>
      )}
    </div>
  );
}

function WorkerProvenanceView({ provenance }: { provenance?: WorkerProvenance | null }) {
  const label = provenance?.status === "verified_exact"
    ? "Verified exact"
    : provenance?.status === "opaque_handle_only"
      ? "Opaque correlation only — not execution proof"
      : provenance?.status === "unavailable"
        ? "Unavailable"
        : "Not reported";
  return (
    <div>
      <div className="label mb-1">Worker Provenance</div>
      <div className={cn("text-xs font-mono", provenance?.status === "verified_exact" ? "text-success" : "text-fg-muted")}>
        {label}
      </div>
      {provenance && (
        <div className="text-[10px] font-mono text-fg-dim mt-1">
          Fabrication guard: {provenance.fabrication_guard ? "active" : "not asserted"}
        </div>
      )}
    </div>
  );
}
