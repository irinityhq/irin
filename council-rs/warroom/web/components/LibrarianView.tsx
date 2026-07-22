"use client";

import { useEffect, useState, useCallback, useRef } from "react";
import { librarian } from "@/lib/librarian";
import { classifyAskFailure } from "@/lib/librarian-abort";
import {
  applyLibrarianEvent,
  emptyPendingTurn,
  openLibrarianAsk,
  type LibrarianPendingTurn,
  type LibrarianSocket,
} from "@/lib/librarian-ws";
import type {
  LibrarianChat, LibrarianChatSummary, LibrarianHealth, LibrarianCabinet,
} from "@/lib/librarian";
import LibrarianSidebar from "./LibrarianSidebar";
import LibrarianTranscript from "./LibrarianTranscript";
import LibrarianComposer from "./LibrarianComposer";
import LibrarianDebugPanel from "./LibrarianDebugPanel";
import { useToast } from "./Toast";

function isRenameConflict(err: unknown): boolean {
  return err instanceof Error && /^409\b/.test(err.message);
}

export default function LibrarianView({
  onOpenSettings,
}: {
  onOpenSettings?: () => void;
}) {
  const [chats, setChats] = useState<LibrarianChatSummary[]>([]);
  const [active, setActive] = useState<LibrarianChat | null>(null);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [cabinetList, setCabinetList] = useState<LibrarianCabinet[]>([]);
  const [cabinet, setCabinet] = useState<string>("research-default");
  const [health, setHealth] = useState<LibrarianHealth | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<LibrarianPendingTurn | null>(null);
  const askAbortRef = useRef<AbortController | null>(null);
  const askSockRef = useRef<LibrarianSocket | null>(null);
  // R20 — feature-detect WS once; sticky per session. Starts true (try WS),
  // flips to false on the first WS failure and stays there for the session so
  // we don't re-probe a broken upgrade on every ask.
  const wsAvailableRef = useRef(true);
  const { toast } = useToast();

  const refreshChats = useCallback(async () => {
    try {
      const r = await librarian.listChats();
      setChats(r.chats);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    librarian.cabinets().then((r) => {
      setCabinetList(r.cabinets);
      if (r.cabinets[0]) setCabinet(r.cabinets[0].name);
    }).catch(() => setCabinetList([]));
    refreshChats();
    const h = setInterval(() => librarian.health().then(setHealth).catch(() => {}), 15000);
    librarian.health().then(setHealth).catch(() => {});
    return () => clearInterval(h);
  }, [refreshChats]);

  useEffect(() => {
    if (!activeId) { setActive(null); return; }
    librarian.getChat(activeId).then((c) => {
      setActive(c);
      setCabinet(c.cabinet);
    }).catch((e) => setError(String(e)));
  }, [activeId]);

  // R20 — tear down any in-flight librarian WS on unmount (close = cancel).
  useEffect(() => () => { askSockRef.current?.close(); }, []);

  async function newConversation() {
    setError(null);
    try {
      const { id } = await librarian.createChat(cabinet);
      await refreshChats();
      setActiveId(id);
    } catch (e) { setError(String(e)); }
  }

  async function renameConversation(id: string, title: string) {
    if (busy) {
      const msg = "Wait for the current ask to finish before renaming";
      setError(msg);
      throw new Error(msg);
    }
    setError(null);
    try {
      const fresh = await librarian.getChat(id);
      const updated = await librarian.renameChat(id, title, fresh.updated_at);
      if (activeId === id) setActive(updated);
      await refreshChats();
    } catch (e) {
      if (isRenameConflict(e)) {
        try {
          const refetched = await librarian.getChat(id);
          if (activeId === id) setActive(refetched);
          await refreshChats();
          toast("error", "Title changed elsewhere — refreshed");
        } catch (refetchErr) {
          setError(String(refetchErr));
        }
        return;
      }
      setError(String(e));
      throw e;
    }
  }

  async function deleteConversation(id: string) {
    setError(null);
    try {
      await librarian.deleteChat(id);
      if (activeId === id) setActiveId(null);
      await refreshChats();
    } catch (e) { setError(String(e)); }
  }

  async function send(content: string) {
    if (!activeId) {
      try {
        const { id } = await librarian.createChat(cabinet);
        setActiveId(id);
        await refreshChats();
        return await sendTo(id, content);
      } catch (e) { setError(String(e)); return; }
    }
    return sendTo(activeId, content);
  }

  /**
   * feature contract / R20: abort the in-flight ask. For the WS path, closing the socket
   * IS the cancel (server treats close as cancel, reusing feature contract semantics).
   * For the POST fallback, the AbortController aborts the fetch. The catch /
   * close paths below recover state.
   */
  function stopAsk() {
    if (askSockRef.current) {
      askSockRef.current.close();
      askSockRef.current = null;
      // Closing mid-stream is a user cancel — mirror the POST-abort UX.
      toast("info", "Ask stopped — reply discarded");
      void recoverAfterStop(activeId);
      setBusy(false);
      setPending(null);
      return;
    }
    askAbortRef.current?.abort();
  }

  async function recoverAfterStop(id: string | null) {
    if (!id) return;
    // The backend already appended the user turn; refetch so it renders as a
    // dangling message (no draft restore — the text lives in the transcript).
    try {
      const fresh = await librarian.getChat(id);
      setActive(fresh);
      await refreshChats();
    } catch {
      // Best-effort; chat reloads on next selection.
    }
  }

  async function sendTo(id: string, content: string) {
    // Fresh id per send — a retry after Stop never reuses an aborted ask's
    // idempotency key (the backend caches only successful asks).
    const clientMsgId = `c_${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
    if (wsAvailableRef.current) {
      return sendViaWs(id, content, clientMsgId);
    }
    return sendViaPost(id, content, clientMsgId);
  }

  /**
   * R20 — stream the ask over /ws/librarian. On any WS failure before a
   * successful completion, flip the sticky fallback flag and retry over POST so
   * the operator never loses an ask to a broken upgrade.
   */
  function sendViaWs(id: string, content: string, clientMsgId: string) {
    setBusy(true);
    setError(null);
    setPending(emptyPendingTurn());
    let settled = false;
    let sawEvent = false;

    const sock = openLibrarianAsk(
      id,
      { text: content, client_msg_id: clientMsgId },
      (ev) => {
        sawEvent = true;
        setPending((prev) => applyLibrarianEvent(prev ?? emptyPendingTurn(), ev));
        if (ev.type === "ask_complete") {
          // Authoritative final turn — persistence confirmed on `done`.
          settled = true;
        } else if (ev.type === "error") {
          settled = true;
          setError(ev.message || "Librarian ask failed");
        }
      },
      (msg) => {
        // Transport error. If we never saw a frame, the upgrade is broken —
        // fall back to POST (sticky for the session).
        if (!sawEvent && !settled) {
          settled = true;
          wsAvailableRef.current = false;
          askSockRef.current = null;
          void sendViaPost(id, content, clientMsgId);
        } else if (!settled) {
          setError(msg);
        }
      },
      () => {
        askSockRef.current = null;
        if (settled) {
          // Normal completion or handled error — persist + clear the preview.
          setPending(null);
          setBusy(false);
          librarian
            .getChat(id)
            .then((fresh) => {
              setActive(fresh);
              return refreshChats();
            })
            .catch(() => {});
        } else if (!sawEvent) {
          // Closed before any frame and the error handler already routed to
          // POST — nothing to do here.
        } else {
          // Closed mid-stream without completion and not via Stop (Stop nulls
          // the ref before close fires) — surface as a dropped connection.
          setPending(null);
          setBusy(false);
        }
      },
    );
    askSockRef.current = sock;
  }

  async function sendViaPost(id: string, content: string, clientMsgId: string) {
    setBusy(true);
    setError(null);
    setPending(null);
    const controller = new AbortController();
    askAbortRef.current = controller;
    try {
      await librarian.ask(id, content, clientMsgId, controller.signal);
      const fresh = await librarian.getChat(id);
      setActive(fresh);
      await refreshChats();
    } catch (e) {
      if (classifyAskFailure(e, controller.signal.aborted) === "aborted") {
        // User pressed Stop — not an error. The backend already appended the
        // user turn; refetch so it renders as a dangling message (no draft
        // restore: the sent text lives in the transcript).
        toast("info", "Ask stopped — reply discarded");
        await recoverAfterStop(id);
      } else {
        setError(String(e));
      }
    } finally {
      if (askAbortRef.current === controller) askAbortRef.current = null;
      setBusy(false);
    }
  }

  return (
    <div data-testid="librarian-shell" className="flex h-[calc(100vh-3rem)] border-t border-border">
      <LibrarianSidebar
        chats={chats}
        activeId={activeId}
        onSelect={setActiveId}
        onNew={newConversation}
        onDelete={deleteConversation}
        onRename={renameConversation}
        renameDisabled={busy}
        health={health}
      />
      <div className="flex-1 flex flex-col min-w-0 bg-bg-deep">
        {error && (
          <div className="border-b border-danger/35 bg-danger/[0.06] text-danger px-4 py-2 text-[11px] font-mono">
            {error}
          </div>
        )}
        {health?.state === "offline" && (
          <div className="border-b border-amber/35 border-l-2 border-l-amber bg-amber/[0.05] text-amber px-4 py-2 text-[11px] font-mono flex items-center gap-3">
            <span>
              Librarian upstream is offline (no service at configured base URL).
              Configure in Settings → Librarian base, then restart council.
            </span>
            {onOpenSettings && (
              <button
                onClick={onOpenSettings}
                className="btn btn-secondary text-xs px-2 py-0.5 whitespace-nowrap"
              >
                Open Settings
              </button>
            )}
          </div>
        )}
        <LibrarianTranscript
          messages={active?.messages ?? []}
          busy={busy}
          pending={pending}
        />
        <LibrarianComposer
          cabinets={cabinetList}
          cabinet={cabinet}
          onCabinetChange={setCabinet}
          onSend={send}
          onStop={stopAsk}
          busy={busy}
          disabled={health?.state === "offline"}
        />
        <LibrarianDebugPanel onError={setError} />
      </div>
    </div>
  );
}
