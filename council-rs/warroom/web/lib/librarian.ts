// Mirrors warroom/backend/librarian/routes.py response shapes.
// Keep in sync.

import { getApiBase, getAuthToken } from "./runtime-config";

export interface LibrarianCabinet {
  name: string;
  description?: string;
}

export interface LibrarianHealth {
  state: "online" | "warming" | "offline" | "unknown";
  model: string | null;
  detail?: string;
  last_check_ts?: number;
}

export interface LibrarianSource {
  path: string;
  score: number;
  snippet: string;
  corpus?: "knowledge" | "identity";
  trust_tier?: number;
}

export interface LibrarianMessage {
  type: "user" | "assistant";
  id: string;
  content: string;
  ts: string;
  client_msg_id?: string;
  sources?: LibrarianSource[];
  model?: string;
  redacted?: boolean;
  partial?: boolean;
}

export interface LibrarianChat {
  id: string;
  cabinet: string;
  title: string;
  created_at: string;
  updated_at: string;
  schema_version: number;
  messages: LibrarianMessage[];
}

export interface LibrarianChatSummary {
  id: string;
  title: string;
  cabinet: string;
  updated_at: string;
  ask_count: number;
}

export interface LibrarianAskResult {
  user_turn: LibrarianMessage;
  assistant_turn: LibrarianMessage;
}

/** Mirrors `adapter::LibrarianContext` (Rust). */
export interface IdentityContext {
  tenant_id: string;
  facts: string[];
}

export interface MemoryContext {
  recent_summaries: string[];
  active_commit?: string | null;
}

export interface LibrarianContext {
  identity: IdentityContext;
  memory: MemoryContext;
}

/** Mirrors `adapter::CommitProposal` (Rust). */
export interface CommitProposal {
  tenant_id: string;
  causal_fire_id: string;
  content: string;
  weight?: number;
}

export interface CommitAck {
  status: string;
  tenant_id: string;
  causal_fire_id: string;
}

function headers(extra: Record<string, string> = {}): Record<string, string> {
  const out: Record<string, string> = { "Content-Type": "application/json", ...extra };
  const token = getAuthToken();
  if (token) out.Authorization = `Bearer ${token}`;
  return out;
}

async function req<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(`${getApiBase()}${path}`, {
    ...init,
    headers: headers(init.headers as Record<string, string> | undefined),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status}: ${text}`);
  }
  if (res.status === 204) return undefined as unknown as T;
  return (await res.json()) as T;
}

export const librarian = {
  health: () => req<LibrarianHealth>("/api/librarian/health"),
  cabinets: () => req<{ cabinets: LibrarianCabinet[] }>("/api/librarian/cabinets"),
  listChats: () => req<{ chats: LibrarianChatSummary[] }>("/api/librarian/chats"),
  getChat: (id: string) => req<LibrarianChat>(`/api/librarian/chats/${id}`),
  createChat: (cabinet: string) =>
    req<{ id: string }>("/api/librarian/chats", {
      method: "POST",
      body: JSON.stringify({ cabinet }),
    }),
  renameChat: (id: string, title: string, ifMatch: string) =>
    req<LibrarianChat>(`/api/librarian/chats/${id}`, {
      method: "PATCH",
      headers: { "If-Match": ifMatch },
      body: JSON.stringify({ title }),
    }),
  deleteChat: (id: string) =>
    req<void>(`/api/librarian/chats/${id}`, { method: "DELETE" }),
  // feature contract: optional AbortSignal threads through `req` into fetch — aborting
  // rejects with DOMException "AbortError" (see lib/librarian-abort.ts). The
  // backend tolerates the mid-ask disconnect; the user turn it already
  // appended surfaces on the next getChat.
  ask: (id: string, content: string, clientMsgId: string, signal?: AbortSignal) =>
    req<LibrarianAskResult>(`/api/librarian/chats/${id}/asks`, {
      method: "POST",
      body: JSON.stringify({ content, client_msg_id: clientMsgId }),
      signal,
    }),
  getContext: (tenant: string) =>
    req<LibrarianContext>(`/api/librarian/context/${encodeURIComponent(tenant)}`),
  postCommit: (body: CommitProposal) =>
    req<CommitAck>("/api/librarian/commits", {
      method: "POST",
      body: JSON.stringify(body),
    }),
};
