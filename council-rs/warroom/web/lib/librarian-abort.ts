/**
 * Librarian Stop-button state machine (feature contract).
 *
 * `librarian.ask()` is a single-JSON fetch; aborting it rejects with a
 * DOMException named "AbortError". The view must NOT surface that rejection
 * in the error banner — the backend tolerates the disconnect (the user turn
 * is already appended server-side), so the correct recovery is: swallow the
 * abort, refetch the chat (the dangling user message renders), and re-enable
 * the composer. A fresh client_msg_id is minted per send, so a retry after
 * Stop never collides with the aborted ask's idempotency key.
 */

export function isAbortError(e: unknown): boolean {
  if (typeof DOMException !== "undefined" && e instanceof DOMException) {
    return e.name === "AbortError";
  }
  return e instanceof Error && e.name === "AbortError";
}

export type AskFailure = "aborted" | "error";

/**
 * Classify a failed ask. `signalAborted` covers environments that reject
 * with a non-AbortError after the controller fired (e.g. wrapped errors).
 */
export function classifyAskFailure(
  e: unknown,
  signalAborted: boolean,
): AskFailure {
  return signalAborted || isAbortError(e) ? "aborted" : "error";
}
