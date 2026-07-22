import type {
  Cabinet, DiscoverResponse, DriftReport, DriftReportListResponse, EmbeddingStats,
  ForkResult, HealthResponse, LineageResponse, MapmakerBrief,
  MapmakerBriefSummary, MapmakerResult, MapPreview, MetaReviewReport,
  MetaReviewResult, PatternsResponse, PrecedentResponse, SeatSwap, SessionIndexEntry, SynthesisDiffResult,
  WeeklySummary,
} from "./types";
import {
  getApiBase,
  getAuthToken,
  getGatewayBase,
  getRuntimeConfig,
  getWsBase,
} from "./runtime-config";

/** @deprecated Use getApiBase() after runtime config is hydrated. */
export function apiBase(): string {
  return getApiBase();
}

/** @deprecated Use getGatewayBase() after runtime config is hydrated. */
export function gatewayBase(): string {
  return getGatewayBase();
}

/** @deprecated Use getWsBase() after runtime config is hydrated. */
export function wsBase(): string {
  return getWsBase();
}

/** WebSocket auth token (Sec-WebSocket-Protocol). */
export function wsAuthToken(): string {
  return getAuthToken();
}

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const token = getAuthToken();
  return token
    ? { ...extra, Authorization: `Bearer ${token}` }
    : extra;
}

/** Extract a human-readable message from failed API responses. */
export async function parseApiErrorBody(res: Response): Promise<string> {
  const fallback = `${res.status} ${res.statusText}`;
  let text: string;
  try {
    text = await res.text();
  } catch {
    return fallback;
  }
  if (!text.trim()) return fallback;
  try {
    const j = JSON.parse(text) as Record<string, unknown>;
    const msg = j.error ?? j.detail ?? j.message;
    if (typeof msg === "string" && msg.trim()) return msg.trim();
    if (Array.isArray(msg)) {
      const parts = msg
        .map((item) => {
          if (typeof item === "string") return item;
          if (item && typeof item === "object" && "msg" in item) {
            return String((item as { msg: unknown }).msg);
          }
          return JSON.stringify(item);
        })
        .filter(Boolean);
      if (parts.length) return parts.join("; ");
    }
  } catch {
    // not JSON — use raw body
  }
  const trimmed = text.trim();
  return trimmed.length > 240 ? `${trimmed.slice(0, 240)}…` : trimmed;
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${getApiBase()}${path}`, {
    cache: "no-store",
    headers: authHeaders(),
  });
  if (!res.ok) {
    const detail = await parseApiErrorBody(res);
    throw new Error(`${res.status} ${detail} on ${path}`);
  }
  return res.json() as Promise<T>;
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${getApiBase()}${path}`, {
    method: "POST",
    headers: authHeaders({ "Content-Type": "application/json" }),
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const detail = await parseApiErrorBody(res);
    throw new Error(`${res.status} ${detail} on ${path}`);
  }
  return res.json() as Promise<T>;
}

export const api = {
  health: () => get<HealthResponse>("/api/health"),
  cabinets: () => get<{ cabinets: Cabinet[] }>("/api/cabinets"),
  // feature contract — name must match ^[a-z0-9][a-z0-9_-]{0,63}$ and yaml must parse
  // as a Rust Cabinet server-side; 4xx errors surface via parseApiErrorBody.
  saveCabinet: (name: string, yaml: string) =>
    post<import("./cabinet-save").CabinetSaveResponse>(
      "/api/cabinets/save", { name, yaml }),
  sessions: (limit = 100) =>
    get<{ sessions: SessionIndexEntry[] }>(`/api/sessions?limit=${limit}`),
  session: (id: string) => get<unknown>(`/api/sessions/${id}`),
  // Defaults mirror the engine's injection parameters (RETRIEVE_THRESHOLD /
  // RETRIEVE_LIMIT) so a bare preview shows what a convene would inject.
  precedent: (q: string, threshold = 0.15, limit = 5,
              mode: "auto" | "semantic" | "keyword" = "auto") =>
    get<PrecedentResponse>(
      `/api/precedent?q=${encodeURIComponent(q)}&threshold=${threshold}` +
      `&limit=${limit}&mode=${mode}`,
    ),
  mapPreview: (dir_path: string) =>
    post<MapPreview>("/api/map/preview", { dir_path }),
  mapmakerRun: (dir_path: string, task: string,
                model: "auto" | "grok" | "gemini" = "auto") =>
    post<MapmakerResult>("/api/mapmaker/run", { dir_path, task, model }),
  mapmakerBriefs: (limit = 50) =>
    get<{ briefs: MapmakerBriefSummary[] }>(
      `/api/mapmaker/briefs?limit=${limit}`),
  mapmakerBrief: (name: string) =>
    get<MapmakerBrief>(`/api/mapmaker/briefs/${encodeURIComponent(name)}`),

  // Phase 2 — Gen 10 intelligence

  embeddingsStats: () => get<EmbeddingStats>("/api/embeddings/stats"),
  embeddingsRebuild: (force = false) =>
    post<{ built: boolean; added?: number; total?: number; reason?: string }>(
      `/api/embeddings/rebuild?force=${force}`, {}),
  precedentReindex: () =>
    post<{ reindexed: number }>("/api/precedent/reindex", {}),

  // Phase 9 N06 — server-rendered PDF of a saved session (application/pdf).
  // Bearer-authed fetch → Blob; the caller triggers the browser download.
  exportSessionPdf: async (id: string): Promise<Blob> => {
    const res = await fetch(`${getApiBase()}/api/sessions/${id}/export/pdf`, {
      method: "POST",
      headers: authHeaders(),
    });
    if (!res.ok) {
      const detail = await parseApiErrorBody(res);
      throw new Error(`${res.status} ${detail} on /api/sessions/${id}/export/pdf`);
    }
    return res.blob();
  },

  fork: (id: string, swaps: SeatSwap[]) =>
    post<ForkResult>(`/api/sessions/${id}/fork`, { swaps }),
  lineage: (id: string) =>
    get<LineageResponse>(`/api/sessions/${id}/lineage`),
  diff: (a: string, b: string) =>
    get<SynthesisDiffResult>(`/api/sessions/${a}/diff/${b}`),

  interventions: (days?: number, limit = 200) => {
    const q = days ? `?days=${days}&limit=${limit}` : `?limit=${limit}`;
    return get<{ entries: import("./types").InterventionEntry[];
                  total: number }>(`/api/interventions${q}`);
  },
  patterns: (days?: number) =>
    get<PatternsResponse>(`/api/patterns${days ? `?days=${days}` : ""}`),

  // Phase 9 N03 — session clusters (k-means over the embedding index).
  clusters: async () => {
    const raw = await get<unknown>("/api/clusters");
    return (await import("./clusters")).normalizeClustersResponse(raw);
  },
  // Phase 9 N04 — proactive intervention probability for current convergence.
  predictIntervention: (convergence: number, round: number) =>
    get<import("./types").InterventionPrediction>(
      `/api/interventions/predict?convergence=${convergence}&round=${round}`,
    ),

  driftReports: () => get<DriftReportListResponse>("/api/drift/reports"),
  driftReport: (name: string) =>
    get<DriftReport>(`/api/drift/reports/${encodeURIComponent(name)}`),
  driftRun: (window: number, limit?: number) =>
    post<{ status: string; window: number; limit?: number }>(
      "/api/drift/run", { window, limit }),

  // Phase 2.1 — weekly summary
  weeklyLatest: () => get<WeeklySummary>("/api/drift/weekly"),
  weeklyHistory: (limit = 12) =>
    get<{ summaries: WeeklySummary[] }>(
      `/api/drift/weekly/history?limit=${limit}`),
  weeklyRun: (window = 7, limit = 8, post_webhooks = false) =>
    post<{ status: string }>("/api/drift/weekly/run",
      { window, limit, post_webhooks }),

  // Meta-review
  metaReviewRun: async (): Promise<MetaReviewResult> => {
    const res = await fetch(`${getApiBase()}/api/meta-review/run`, {
      method: "POST",
      headers: authHeaders({ "Content-Type": "application/json" }),
      body: "{}",
    });
    if (res.status === 409)
      return { status: "error", error: "Meta-review already in progress" };
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return res.json() as Promise<MetaReviewResult>;
  },
  metaReviewLatest: () => get<MetaReviewReport>("/api/meta-review/latest"),

  // Phase 6 — provider discovery (feature contract)
  discover: () => get<DiscoverResponse>("/api/discover"),

  // Gate 4 — Council-authenticated BFF. Gateway admin credentials never
  // enter browser configuration or response data.
  governanceWatch: () =>
    get<import("./watch-gateway").WatchSnapshot>("/api/governance/watch"),
  governanceOutbox: () =>
    get<import("./governance").OutboxListResponse>("/api/governance/outbox"),
  governanceOutboxDetail: (id: string) =>
    get<import("./governance").OutboxDetailResponse>(
      `/api/governance/outbox/${encodeURIComponent(id)}`,
    ),
  governanceOutboxPubkey: () =>
    get<import("./governance").OutboxPubkey>("/api/governance/outbox/pubkey"),

};

/** Re-export for components that need the full config object. */
export { getRuntimeConfig };
