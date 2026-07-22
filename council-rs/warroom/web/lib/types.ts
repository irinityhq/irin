// Mirrors the StreamEvent schema in council_stream.py at the project root.
// Keep in sync when adding event types.

export type StreamEventType =
  | "session_started"
  | "precedent_loaded"
  | "info"
  | "round_validation"
  | "round_started"
  | "seat_started"
  | "seat_chunk"
  | "seat_complete"
  | "convergence_scored"
  | "round_divergence"
  | "round_complete"
  | "awaiting_input"
  | "intervention_received"
  | "specops_started"
  | "specops_signal"
  | "synthesis_started"
  | "synthesis_complete"
  | "session_saved"
  | "budget_paused"
  | "phase_started"
  | "done"
  | "error";

export interface StreamEvent<T = unknown> {
  type: StreamEventType;
  session_id: string;
  ts: string;
  data: T;
}

export interface SeatRef {
  name: string;
  provider: string;
  model: string;
}

export interface ProviderProvenance {
  runner: string;
  access_mode: string;
  accounting: string;
  filesystem: string;
}

export interface GatewayProvenance {
  gateway_request_id: string;
  routed_model: string;
  routed_provider: string;
  fallback_used: boolean;
}

export type ExecutionRoute = "direct" | "governed" | "unknown";

export interface SessionStartedData {
  topic: string;
  cabinet_name: string;
  rounds_planned: number;
  mode: "blind" | "normal";
  active_seats: SeatRef[];
  dropped_seats: { name: string; provider: string }[];
  chair: { provider: string; model: string };
  available_providers: string[];
  council_version: string;
  stream_version: string;
  tier?: string;
  phase?: number;
  phases_total?: number;
  deliberation_mode?: "pathfind" | "teardown" | "harden";
  /** feature contract: emitted by both the real path and the smoke shim. */
  via_gateway?: boolean;
  /** Effective backend-enforced transport, not merely the requested toggle. */
  execution_route?: ExecutionRoute;
  /** Lowercased `VALID_SENSITIVITY_LEVELS` (provider/mod.rs). */
  sensitivity?: "green" | "yellow" | "red";
  /** Direct-fire slug (feature contract) — only on direct-fire payloads. */
  direct_fire?: string;
}

export interface PrecedentMatch {
  id: string;
  ts: string;
  topic: string;
  keywords: string[];
  ruling_digest: string;
  confidence: string;
  cabinet: string;
  convergence: number;
  mode: string;
  /** Fused relevance from the unified retriever (hybrid-v1 / keyword-v1).
   *  Present on /api/precedent matches and precedent_loaded WS events. */
  score?: number;
  /** Why it matched, e.g. "semantic 0.62 · keyword 0.25". */
  why?: string;
}

/**
 * N01 token streaming (Phase 9). Emitted between `seat_started` and
 * `seat_complete` for streaming-capable providers (openai_compat SSE / grok
 * native). Non-streaming providers emit zero chunks — that is always legal.
 * `seat_complete.text` stays authoritative: the UI replaces accumulated chunk
 * deltas with it on completion.
 */
export interface SeatChunkData {
  seat_name: string;
  round_num: number;
  text_delta: string;
  /** Monotonic per-seat sequence. mpsc preserves order; used defensively. */
  seq: number;
}

export interface SeatCompleteData {
  seat_name: string;
  provider: string;
  model: string;
  text: string;
  round_num: number;
  latency_ms: number;
  tokens_in: number;
  tokens_out: number;
  cached_in: number;
  cost_usd: number;
  /** Omitted on the wire for successful seats (Rust `skip_serializing_if`). */
  error?: string | null;
  provider_provenance?: ProviderProvenance | null;
  /** Rust `SeatResponse.gateway`; legacy clients used `gateway_provenance`. */
  gateway?: GatewayProvenance | null;
  gateway_provenance?: GatewayProvenance | null;
}

export interface ConvergenceScoredData {
  round_num: number;
  score: number;
  converged: boolean;
}

/** One seat projected to 2D for the N02 divergence scatter. */
export interface DivergencePoint {
  seat: string;
  x: number;
  y: number;
}

/**
 * N02 divergence map (Phase 9). Emitted after `convergence_scored` when the
 * backend can embed each seat response. `method` is "pca" (hand-rolled
 * 2-component PCA over fastembed vectors — UMAP has no mature Rust crate, so
 * the method is labelled truthfully). Omitted entirely when embeddings are
 * unavailable; the UI tolerates absence.
 */
export interface RoundDivergenceData {
  round_num: number;
  method: string;
  points: DivergencePoint[];
}

export interface AwaitingInputData {
  round_num: number;
  convergence: number;
  converged: boolean;
  options: InterventionAction[];
  /** Present after a manual escalation — the signal text to review. */
  specops_signal?: string;
}

export interface SpecopsSignalData {
  text: string;
  model: string;
  latency_ms: number;
  cost_usd: number;
  tokens_in?: number;
  tokens_out?: number;
  mode?: string;
  error: string | null;
}

export interface SynthesisCompleteData {
  text: string;
  model: string;
  latency_ms: number;
  cost_usd: number;
  provider_provenance?: ProviderProvenance | null;
}

export interface BudgetPausedData {
  round_num: number;
  total_cost_usd: number;
  max_usd: number;
  action: "end_early";
}

export interface PhaseStartedData {
  phase: number;
  label: string;
  parent_session_id: string;
}

export interface PhaseSummary {
  phase: number;
  session_id: string;
  deliberation_mode: string;
  rounds_run: number;
  convergence_final?: number;
  total_cost_usd?: number;
  budget_paused?: boolean;
}

export interface DoneData {
  total_tokens: number;
  total_cost_usd: number;
  total_latency_ms: number;
  synthesis: string;
  session_id: string;
  convergence_final: number;
  rounds_run: number;
  phases_completed?: number;
  phases_total?: number;
  phase_summaries?: PhaseSummary[];
}

export type InterventionAction =
  | "continue"
  | "end_early"
  | "escalate_specops"
  | "escalate_munger"
  | "escalate_contrarian"
  | "escalate_kiss"
  | "inject_context"
  | "swap_seat";

export interface InterventionPayload {
  action: InterventionAction;
  text?: string;
  seat_name?: string;
  provider?: string;
  model?: string;
  system?: string;
}

// REST types

export interface MapmakerResult {
  model: "grok" | "gemini";
  model_id: string;
  map: string;
  task: string;
  directory: string;
  file_count: number;
  bundle_bytes: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd: number;
  latency_ms: number;
  brief_filename: string | null;
  brief_path: string | null;
  error?: string;
}

export interface MapmakerBriefSummary {
  name: string;
  size: number;
  mtime: string;
}

export interface MapmakerBrief {
  name: string;
  content: string;
  mtime: string;
}

export interface CabinetSeat {
  name: string;
  provider: string;
  model: string;
  system: string;
  system_source?: string;
}

export interface Cabinet {
  name: string;
  label: string;
  description: string;
  seats: CabinetSeat[];
  chair: {
    name?: string;
    provider: string;
    model: string;
    /** Optional inline chair system prompt (Rust `Chair.system`). */
    system?: string;
    system_source?: string;
    /** Thinking effort for adaptive providers: low|medium|high|max. */
    thinking_effort?: string;
  };
  rounds: number;
  is_triad: boolean;
  local_code_only?: boolean;
  /** Chair synthesis output contract (Rust `SynthesisMode`, serde snake_case). */
  synthesis_mode?: "generic" | "directive_proposal_v1";
}

export interface SessionResponse {
  seat_name: string;
  provider: string;
  model: string;
  text: string;
  round_num: number;
  latency_ms: number;
  tokens_in: number;
  tokens_out: number;
  cached_in: number;
  cost_usd: number;
  /** Omitted on the wire for successful seats (Rust `skip_serializing_if`). */
  error?: string | null;
  provider_provenance?: ProviderProvenance | null;
  gateway?: GatewayProvenance | null;
}

export interface SessionRound {
  round_num: number;
  responses: SessionResponse[];
  convergence_score: number | null;
  converged: boolean;
  /** Gateway ledger handles for all convergence-judge cascade attempts. */
  judge_gateway_attempts?: GatewayProvenance[];
  /** Validation report from Sheldon if --validate was used. */
  validation_report?: Array<{
    claim: string;
    seat: string;
    verdict: "SUPPORTED" | "CONSISTENT" | "NO_EVIDENCE" | "CONTRADICTED";
    confidence?: number;
    impact?: "HIGH" | "MEDIUM" | "LOW" | "UNKNOWN";
    evidence_citations?: string[];
    reasoning?: string;
  }>;
}

/**
 * Wire values of the Rust `SessionMode` enum (serde lowercase).
 * "normal" is the legacy/default mode; unknown strings deserialize to
 * "unknown" on the Rust side before reaching this layer.
 */
export type SessionMode =
  | "normal"
  | "teardown"
  | "pathfind"
  | "harden"
  | "blind"
  | "recall"
  | "wargame"
  | "premortem"
  | "contrarian"
  | "munger"
  | "kiss"
  | "specops"
  | "unknown";

export interface SessionDetail {
  session_id: string;
  topic: string;
  cabinet_name: string;
  rounds: SessionRound[];
  /** Omitted on the wire for sessions saved without a synthesis. */
  synthesis?: string;
  /** Omitted on the wire when no synthesis model ran. */
  synthesis_model?: string;
  chair_provider_provenance?: ProviderProvenance | null;
  chair_gateway_provenance?: GatewayProvenance | null;
  execution_route?: ExecutionRoute;
  gateway_sensitivity?: string | null;
  total_tokens: number;
  total_latency_ms: number;
  total_cost_usd: number;
  mode: SessionMode;
  precedent_ids: string[];
  timestamp: string;
  origin?: string;
  parent_request_id?: string;
}

export interface SessionIndexEntry {
  id: string;
  ts: string;
  topic: string;
  keywords: string[];
  ruling_digest: string;
  confidence: string;
  cabinet: string;
  convergence: number;
  mode: SessionMode;
  execution_route?: ExecutionRoute;
  gateway_sensitivity?: string | null;
  seat_count: number;
  rounds: number;
  /** Always present — the list endpoint backfills "" when missing. */
  synthesis_model: string;
  version: string;
  origin?: string;
  parent_request_id?: string;
}

export interface HealthResponse {
  council_version: string;
  /** Compile-time Council source identity; older running binaries may omit it. */
  build_sha?: string;
  build_dirty?: boolean;
  stream_version: string;
  providers_available: string[];
  providers_missing: string[];
  sessions_dir: string;
  index_path: string;
  index_exists: boolean;
  ws_smoke_only?: boolean;
}

export interface MapPreview {
  directory: string;
  file_count: number;
  files: string[];
  total_bytes: number;
  preview: string;
  error?: string;
}

// In-flight UI state

export type WarRoomPhase =
  | "idle"
  | "connecting"
  | "streaming"
  | "paused"
  | "specops"
  | "synthesizing"
  | "done"
  | "error";

export interface SeatRuntimeState {
  seat: SeatRef;
  text: string;
  status: "pending" | "thinking" | "complete" | "error";
  /** N01 — true while seat_chunk deltas are arriving (before seat_complete). */
  streaming?: boolean;
  /** N01 — highest seat_chunk seq applied; guards out-of-order/duplicate. */
  last_seq?: number;
  latency_ms: number;
  tokens_in: number;
  tokens_out: number;
  cached_in: number;
  cost_usd: number;
  error: string | null;
  round_num: number;
  provider_provenance?: ProviderProvenance | null;
  gateway_provenance?: GatewayProvenance | null;
}

export interface RoundValidationData {
  round_num: number;
  gate_applied: boolean;
  verdicts: Array<{
    claim: string;
    seat: string;
    verdict: "SUPPORTED" | "CONSISTENT" | "NO_EVIDENCE" | "CONTRADICTED";
    confidence: number;
    impact: "HIGH" | "MEDIUM" | "LOW" | "UNKNOWN";
    evidence_citations: string[];
    reasoning: string;
  }>;
}

export interface RoundRuntimeState {
  round_num: number;
  seats: Record<string, SeatRuntimeState>; // by seat name
  convergence?: number;
  converged?: boolean;
  early_convergence?: boolean;
  complete: boolean;
  validation?: RoundValidationData;
  /** N02 — 2D PCA projection of seat responses. Absent when unavailable. */
  divergence?: DivergencePoint[];
}

export interface DeliberationState {
  phase: WarRoomPhase;
  session_id: string;
  topic: string;
  cabinet_label: string;
  cabinet_name: string;
  rounds_planned: number;
  mode: "blind" | "normal";
  active_seats: SeatRef[];
  dropped_seats: { name: string; provider: string }[];
  chair: { provider: string; model: string };
  execution_route: ExecutionRoute;
  gateway_sensitivity?: string;
  precedent: PrecedentMatch[];
  rounds: RoundRuntimeState[];
  current_round: number;
  awaiting?: AwaitingInputData;
  /** Set when an intervention is sent; cleared when the next phase event lands. */
  pendingIntervention?: "end_early" | null;
  specops?: SpecopsSignalData & { trigger?: "auto" | "manual" };
  synthesis?: SynthesisCompleteData;
  totals: { tokens: number; cost_usd: number; latency_ms: number };
  errors: { message: string; ts: string; fatal: boolean }[];
  info_messages: { message: string; ts: string }[];
  saved_path?: string;
  budget_paused?: BudgetPausedData;
  phase_label?: string;
  stream_phase?: number;
  stream_phases_total?: number;
  deliberation_mode?: string;
  tier?: string;
}

// ───────── Phase 2: Gen 10 intelligence types ─────────

export interface PrecedentMatchSemantic extends PrecedentMatch {
  /** Legacy pure-cosine similarity from the pre-unified semantic preview. */
  similarity?: number;
}

export interface PrecedentResponse {
  query: string;
  mode: "semantic" | "keyword";
  /** Exact ranker identity: "hybrid-v1" | "keyword-v1". */
  engine?: string;
  threshold?: number;
  matches: PrecedentMatchSemantic[];
}

export interface EmbeddingStats {
  available: boolean;
  reason?: string;
  engine_mode?: "semantic" | "keyword";
  present?: boolean;
  session_count?: number;
  vector_dim?: number;
  size_bytes?: number;
  session_index_count?: number;
  stale?: boolean;
  model?: string;
  device?: string;
  path?: string;
}

export interface ForkResult {
  topic: string;
  cabinet: Cabinet;
  parent_id: string;
  parent_cabinet_label: string;
  parent_cabinet_key: string;
  swaps_applied: {
    seat_name: string;
    before: { provider: string; model: string; system: string };
    after: { provider: string; model: string; system: string };
  }[];
}

export interface SeatSwap {
  seat_name: string;
  provider?: string;
  model?: string;
  system?: string;
}

export interface LineageRecord {
  child_id: string;
  parent_id: string;
  swaps: SeatSwap[];
  cabinet_label: string;
  ts: string;
}

export interface LineageResponse {
  session_id: string;
  parent: LineageRecord | null;
  children: LineageRecord[];
}

export interface SynthesisDiffResult {
  similarity: number | null;
  jaccard: number;
  drift: number;
  parent_confidence: string | null;
  child_confidence: string | null;
  confidence_changed: boolean;
  parent_word_count: number;
  child_word_count: number;
  diff_lines: string[];
  unique_to_parent: string[];
  unique_to_child: string[];
  parent_synthesis: string;
  child_synthesis: string;
  parent_id: string;
  child_id: string;
}

export interface InterventionEntry {
  session_id: string;
  action: string;
  payload: Record<string, unknown>;
  round_num: number;
  convergence_at_pause: number;
  ts: string;
  logged_at: string;
}

export interface PatternsResponse {
  total: number;
  session_count: number;
  actions: Record<string, number>;
  by_round: Record<string, number>;
  by_cabinet: Record<string, Record<string, number>>;
  convergence_buckets: Record<string, number>;
  avg_convergence_at_pause: number;
  top_keywords: [string, number][];
  sequences: string[][];
  multi_intervention_sessions: number;
  recent: InterventionEntry[];
  window_days: number | null;
}

export interface DriftReportSummary {
  name: string;
  size: number;
  mtime: string;
}

export interface DriftReport {
  name: string;
  content: string;
  mtime: string;
}

export interface DriftReportListResponse {
  reports: DriftReportSummary[];
  running: boolean;
}

export interface AnchoringPattern {
  keyword: string;
  avg_drift: number;
  session_count: number;
  score: number;
}

export interface WeeklyHeadline {
  session_id: string;
  topic: string;
  drift_score: number;
  confidence_normal: string | null;
  confidence_blind: string | null;
  confidence_changed: boolean;
}

export interface WeeklySummary {
  ts: string;
  window_days: number;
  sessions_analyzed: number;
  avg_drift: number;
  confidence_flips: number;
  high_drift_count: number;
  top_anchoring: AnchoringPattern[];
  report_filename: string | null;
  report_path: string | null;
  headline_session?: WeeklyHeadline | null;
  webhooks?: Record<string, string>;
  reason?: string;
  error?: string;
}

// ───────── Phase 6: CLI parity types ─────────

/**
 * One provider row from `GET /api/discover` (feature contract).
 * `env_hint` is an env var NAME only (e.g. "XAI_API_KEY") — the backend
 * never sends values or key fragments.
 */
export interface DiscoverProvider {
  name: string;
  /** Human-readable transport label. Falls back to `name` for older servers. */
  label: string;
  /** Provider company/family (for example `xai` or `openai`). */
  family: string;
  /** Concrete execution transport (for example `api`, `grok_build`, or `hermes`). */
  transport: string;
  available: boolean;
  /** Gateway has a concrete adapter for this exact transport. */
  gateway_supported?: boolean;
  source: string;
  env_hint: string | null;
  models: string[];
}

export interface DiscoverResponse {
  providers: DiscoverProvider[];
  log: string[];
}

// ───────── N03: session clusters ─────────

/**
 * One cluster from `GET /api/clusters` (Phase 9 N03). Hand-rolled k-means over
 * the existing session embedding index; `top_terms` come from a tf-idf-ish
 * keyword count of member topics. `session_ids` is capped at 50 by the server.
 */
export interface SessionCluster {
  id: number;
  size: number;
  top_terms: string[];
  session_ids: string[];
}

export interface ClustersResponse {
  clusters: SessionCluster[];
  method: string;
  k: number;
  n_sessions: number;
  generated_at: string;
}

// ───────── N04: intervention prediction ─────────

/**
 * `GET /api/interventions/predict` (Phase 9 N04). Backend trains at request
 * time from intervention_log.jsonl. `method` is "logreg" when >= 30 usable
 * samples exist, else "frequency" (overall escalation rate fallback).
 */
export interface InterventionPrediction {
  probability: number;
  method: "logreg" | "frequency";
  n_samples: number;
}

// ───────── Meta-review types ─────────

export interface MetaReviewResult {
  report_path?: string;
  status: "complete" | "insufficient_data" | "no_drift_data" | "write_failed" | "error";
  weeks?: number;
  mean_drift?: number;
  stability?: string;
  recommendation_preview?: string;
  error?: string;
}

export interface MetaReviewReport {
  name: string;
  content: string;
  mtime: string;
}

export const initialState: DeliberationState = {
  phase: "idle",
  session_id: "",
  topic: "",
  cabinet_label: "",
  cabinet_name: "",
  rounds_planned: 0,
  mode: "normal",
  active_seats: [],
  dropped_seats: [],
  chair: { provider: "", model: "" },
  execution_route: "unknown",
  precedent: [],
  rounds: [],
  current_round: 0,
  totals: { tokens: 0, cost_usd: 0, latency_ms: 0 },
  errors: [],
  info_messages: [],
};
