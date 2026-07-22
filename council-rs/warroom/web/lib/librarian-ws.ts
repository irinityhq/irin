// R20 — librarian WebSocket streaming client (Phase 9).
//
// Mirrors lib/ws.ts conventions. The server emits, per ask:
//   {type:"ask_started"}
//   {type:"ask_chunk", text_delta} (zero or more)
//   {type:"sources", sources:[...]} (if any)
//   {type:"ask_complete", message:{...full turn...}}
//   {type:"done"} | {type:"error", message}
//
// The librarian upstream is a single buffered POST (no partial streaming), so
// the COMPLIANT honest path is ask_started -> (zero chunks) -> sources ->
// ask_complete -> done. The UI MUST render zero-chunk flows identically to a
// chunked flow — the pending turn simply fills in at ask_complete.
//
// Stop closes the WS; the server treats close as cancel (feature contract cancel-safety).
// POST /ask stays as the automatic fallback when the WS is unavailable.

import { getAuthToken, getWsBase } from "./runtime-config";
import type { LibrarianMessage, LibrarianSource } from "./librarian";

export type LibrarianWsEvent =
  | { type: "ask_started" }
  | { type: "ask_chunk"; text_delta: string }
  | { type: "sources"; sources: LibrarianSource[] }
  | { type: "ask_complete"; message: LibrarianMessage }
  | { type: "done" }
  | { type: "error"; message: string };

/** Pending assistant turn rendered while streaming, before ask_complete. */
export interface LibrarianPendingTurn {
  /** Accumulated ask_chunk deltas. Empty for zero-chunk flows. */
  text: string;
  /** Sources, populated on the `sources` frame (if any). */
  sources: LibrarianSource[];
  /** True between ask_started and ask_complete/done/error. */
  streaming: boolean;
}

export function emptyPendingTurn(): LibrarianPendingTurn {
  return { text: "", sources: [], streaming: false };
}

/**
 * Pure reducer for the librarian streaming transcript turn. Exported for unit
 * tests. `ask_complete` carries the authoritative message; chunked text is a
 * preview that gets replaced. Zero-chunk flows leave `text` empty until
 * `ask_complete` swaps in the final message — identical render to today.
 */
export function applyLibrarianEvent(
  turn: LibrarianPendingTurn,
  ev: LibrarianWsEvent,
): LibrarianPendingTurn {
  switch (ev.type) {
    case "ask_started":
      return { text: "", sources: [], streaming: true };
    case "ask_chunk":
      return { ...turn, text: turn.text + (ev.text_delta ?? "") };
    case "sources":
      return { ...turn, sources: Array.isArray(ev.sources) ? ev.sources : [] };
    case "ask_complete":
      return { ...turn, streaming: false };
    case "done":
    case "error":
      return { ...turn, streaming: false };
    default:
      return turn;
  }
}

export interface LibrarianSocket {
  close: () => void;
  ws: WebSocket;
}

/**
 * Open the librarian WS for a chat and send one ask. Mirrors openDeliberation:
 * same subprotocol token array (`["council", "token.<t>"]`). `onEvent` receives
 * each decoded frame; `onError` for transport/decoding failures; `onClose` when
 * the socket closes (used by callers to drive fallback).
 */
export function openLibrarianAsk(
  chatId: string,
  ask: { text: string; client_msg_id: string },
  onEvent: (ev: LibrarianWsEvent) => void,
  onError: (msg: string) => void,
  onClose: () => void,
): LibrarianSocket {
  const url = `${getWsBase()}/ws/librarian/${encodeURIComponent(chatId)}`;
  const token = getAuthToken();
  const protocols = token ? ["council", `token.${token}`] : undefined;
  const ws = protocols ? new WebSocket(url, protocols) : new WebSocket(url);

  ws.addEventListener("open", () => {
    ws.send(
      JSON.stringify({
        type: "ask",
        text: ask.text,
        client_msg_id: ask.client_msg_id,
      }),
    );
  });

  ws.addEventListener("message", (msg) => {
    try {
      onEvent(JSON.parse(msg.data) as LibrarianWsEvent);
    } catch (e) {
      onError(`Malformed librarian event: ${(e as Error).message}`);
    }
  });

  ws.addEventListener("error", () => onError("Librarian WebSocket error"));
  ws.addEventListener("close", () => onClose());

  return {
    ws,
    close: () => {
      try {
        ws.close();
      } catch {
        /* noop */
      }
    },
  };
}
