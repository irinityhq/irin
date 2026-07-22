export type WorkerProvenance = {
  status: "opaque_handle_only" | "verified_exact" | "unavailable";
  fabrication_guard: boolean;
  opaque_handle?: string;
};

export type OutboxSummary = {
  id: string;
  status: string;
  verdict: string;
  authority: string;
  created_at_ms: number;
  signature: {
    alg: string;
    kid: string;
    value: string;
  };
  council_session_id?: string | null;
  council_cost_usd?: number | null;
  expires_at_ms?: number | null;
  acked_at_ms?: number | null;
  worker_provenance?: WorkerProvenance | null;
};

export type OutboxRecord = OutboxSummary & {
  in_response_to: string;
  tenant: string;
  envelope: unknown;
  envelope_json_canonical: string;
};

export type OutboxListResponse = {
  canary_tenant: string;
  directives: OutboxSummary[];
  next_cursor: string | null;
};

export type SignatureVerification = {
  verified: boolean;
  algorithm: "Ed25519";
  kid: string | null;
  detail: string;
};

export type OutboxDetailResponse = {
  directive: OutboxRecord;
  verification: SignatureVerification;
};

export type OutboxPubkey = {
  alg: "Ed25519";
  kid: string;
  pubkey_b64: string;
};

/** Pin the live sidecar list contract; stale `records` projections are invalid. */
export function parseOutboxList(value: unknown): OutboxListResponse {
  if (!value || typeof value !== "object") throw new Error("invalid outbox response");
  const obj = value as Record<string, unknown>;
  if (!Array.isArray(obj.directives)) throw new Error("outbox response missing directives");
  if (typeof obj.canary_tenant !== "string" || !obj.canary_tenant) {
    throw new Error("outbox response missing canary tenant");
  }
  return value as OutboxListResponse;
}
