//! RequestContext — per-request metadata threaded engine→provider for the
//! Phase 0.5 council endpoint (§4.5, §6.5 P0 #5).
//!
//! For CLI / warroom callers `RequestContext::default()` yields empty fields,
//! and the gateway provider client skips emitting the optional headers.

/// Per-deliberation request metadata. Cheap to clone — all fields are owned
/// `String` / primitive.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    /// Parent gateway request ID. Threaded as `X-Parent-Request-Id` on seat
    /// calls so the gateway ledger can attribute seat cost to its wrapper
    /// (§6.4). `None` for CLI/warroom; `Some(uuid)` for `Api` origin and any
    /// nested invocation.
    pub parent_request_id: Option<String>,

    /// Council session ID. Set by the engine after minting; available to
    /// providers via `ctx.council_session_id` if they want to surface it on
    /// outgoing headers.
    pub council_session_id: Option<String>,

    /// Current council depth — informational only here; reentry prevention is
    /// enforced gateway-side per §5.6. Always 0 from the API handler.
    pub depth: u32,

    /// Whether SpecOps auto-escalation is allowed (v0.2).
    pub council_auto_escalate: bool,

    /// Per-session gateway routing override (feature contract). `Some(true)` routes this
    /// request through the Gateway even when the process-wide
    /// `COUNCIL_VIA_GATEWAY` / `--via-gateway` state is off; `Some(false)`
    /// forces direct provider calls. `None` falls back to the process default.
    pub via_gateway: Option<bool>,

    /// Per-session sensitivity level for the `X-Sensitivity-Level` gateway
    /// header — UPPERCASE ("GREEN" | "YELLOW" | "RED"), normalized from the
    /// lowercase WS wire values at parse time. `None` falls back to the
    /// process-wide sensitivity (default "GREEN").
    pub sensitivity: Option<String>,
}
