"use client";

import { useState } from "react";
import { ChevronDown, ChevronRight, Loader2 } from "lucide-react";
import {
  librarian,
  type CommitProposal,
} from "@/lib/librarian";
import { cn } from "@/lib/cn";
import { warroomDebugEnabled } from "@/lib/warroom-debug";

export default function LibrarianDebugPanel({
  onError,
}: {
  onError: (msg: string | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [tenant, setTenant] = useState("system");
  const [contextRaw, setContextRaw] = useState<string | null>(null);
  const [loadingCtx, setLoadingCtx] = useState(false);
  const [commitTenant, setCommitTenant] = useState("system");
  const [causalFireId, setCausalFireId] = useState("");
  const [commitContent, setCommitContent] = useState("");
  const [commitWeight, setCommitWeight] = useState("1");
  const [commitResult, setCommitResult] = useState<string | null>(null);
  const [postingCommit, setPostingCommit] = useState(false);

  if (!warroomDebugEnabled()) return null;

  async function loadContext() {
    setLoadingCtx(true);
    onError(null);
    setContextRaw(null);
    try {
      const ctx = await librarian.getContext(tenant.trim() || "system");
      setContextRaw(JSON.stringify(ctx, null, 2));
    } catch (e) {
      onError(String(e));
    } finally {
      setLoadingCtx(false);
    }
  }

  async function submitCommit() {
    setPostingCommit(true);
    onError(null);
    setCommitResult(null);
    const body: CommitProposal = {
      tenant_id: commitTenant.trim() || "system",
      causal_fire_id: causalFireId.trim() || `manual-${Date.now()}`,
      content: commitContent.trim(),
      weight: Number.parseFloat(commitWeight) || 0,
    };
    if (!body.content) {
      onError("Commit content is required");
      setPostingCommit(false);
      return;
    }
    try {
      const ack = await librarian.postCommit(body);
      setCommitResult(JSON.stringify(ack, null, 2));
    } catch (e) {
      onError(String(e));
    } finally {
      setPostingCommit(false);
    }
  }

  return (
    <div className="border-t border-border bg-bg-deep">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="w-full flex items-center gap-2 px-4 py-2 text-[11px] font-mono text-fg-muted hover:text-fg transition-colors"
      >
        {open ? <ChevronDown className="w-3.5 h-3.5" /> : <ChevronRight className="w-3.5 h-3.5" />}
        Debug (context / commit)
      </button>
      <p className="px-4 pb-1 text-[10px] text-fg-dim font-mono">
        Adapter stubs (v0.3) — context/commit ACK only; not live Librarian memory.
      </p>
      {open && (
        <div className="px-4 pb-4 space-y-4 text-xs font-mono">
          <div className="cg-command-panel cg-command-panel--tight space-y-2">
            <div className="label">
              Tenant context <span className="text-fg-muted normal-case tracking-normal">(stub)</span>
            </div>
            <div className="flex gap-2">
              <input
                value={tenant}
                onChange={(e) => setTenant(e.target.value)}
                placeholder="tenant"
                className="input flex-1 text-xs"
              />
              <button
                type="button"
                onClick={loadContext}
                disabled={loadingCtx}
                className="btn shrink-0"
              >
                {loadingCtx
                  ? <Loader2 className="w-3 h-3 animate-spin" />
                  : "Load context"}
              </button>
            </div>
            {contextRaw && (
              <pre className={cn(
                "max-h-40 overflow-auto p-2 rounded border border-border",
                "bg-bg-deep text-fg-muted whitespace-pre-wrap",
              )}>
                {contextRaw}
              </pre>
            )}
          </div>
          <div className="cg-command-panel cg-command-panel--tight space-y-2">
            <div className="label">
              Manual commit <span className="text-fg-muted normal-case tracking-normal">(stub ACK)</span>
            </div>
            <input
              value={commitTenant}
              onChange={(e) => setCommitTenant(e.target.value)}
              placeholder="tenant_id"
              className="input w-full text-xs"
            />
            <input
              value={causalFireId}
              onChange={(e) => setCausalFireId(e.target.value)}
              placeholder="causal_fire_id (optional)"
              className="input w-full text-xs"
            />
            <textarea
              value={commitContent}
              onChange={(e) => setCommitContent(e.target.value)}
              placeholder="content"
              rows={3}
              className="input w-full text-xs resize-y"
            />
            <input
              value={commitWeight}
              onChange={(e) => setCommitWeight(e.target.value)}
              placeholder="weight"
              className="input w-full text-xs"
            />
            <button
              type="button"
              onClick={submitCommit}
              disabled={postingCommit}
              className="btn btn-primary w-full"
            >
              {postingCommit
                ? <><Loader2 className="w-3 h-3 animate-spin" /> Posting…</>
                : "POST commit"}
            </button>
            {commitResult && (
              <pre className="p-2 rounded border border-border bg-bg-deep text-success whitespace-pre-wrap">
                {commitResult}
              </pre>
            )}
          </div>
        </div>
      )}
    </div>
  );
}