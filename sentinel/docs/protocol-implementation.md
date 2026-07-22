# sovereign-protocol implementation notes

Product intent spine: [`../COMMS_CONTRACT.md`](../COMMS_CONTRACT.md) (v0.1 frozen intent).
This page documents **crate behavior** the contract does not spell out.

---

## Role

Shared **wire types and canonicalization** used by gateway outbox signing/verify and council emitters.
**Signing keys live in gateway**; this crate supplies envelopes, JCS bytes, fence vectors, and provenance/problem shapes.

Package: `sovereign-protocol` (see `Cargo.toml` for version SSOT).

---

## Public modules

| Module | Job |
| --- | --- |
| `comms` | CloudEvents-shaped envelope + builder |
| `directive` | Directive payload type |
| `escalation` | Escalation, Urgency, SentinelState |
| `jcs` | RFC 8785 JSON Canonicalization |
| `fence_vectors` | Golden directive fence cases |
| `types` | Provenance, CapabilityToken, ProblemDetails, seat/provider responses |

Crate root re-exports envelope builder types + Directive + Escalation family; **`types::*` is not all re-exported at root** (import `sovereign_protocol::types::…`).

---

## Envelope spine (also in COMMS_CONTRACT)

Constants (enforce-level in tests):

- `COMMS_CONTRACT_VERSION` = `irin.comms.v0.1`
- `CE_SPECVERSION` = `1.0`
- `CE_DATACONTENTTYPE` = `application/json`
- `ENVELOPE_SCHEMA_VERSION` = `1`

**Kinds:** `Escalation` | `Directive`
**CE type ids:** `irin.escalation.v0.1` | `irin.directive.v0.1`

**CommsData** fields (product spine): contract, kind, tenant, ttl_seconds, budget_hint, reply_to, payload — see COMMS_CONTRACT table.

### Outer wrapper

Wire packaging includes the outer object shape
**`{"v":1,"envelope":…}`** (`EnvelopeWrapper`). Consumers must parse this
wrapper before handling the CloudEvents-shaped envelope.

### Builder / reject behavior

- Builder `build()` errors identify any missing required field:
  `sentinel_name`, `tenant`, `ttl_seconds`, `budget_hint`, or `reply_to`.
  The 32-hex-character id and RFC3339 `Z` time are generated defaults.
- Deserialization rejects an invalid CloudEvents `specversion`,
  `datacontenttype`, or `type`; it fails closed rather than accepting a
  best-effort envelope.

---

## JCS (signing-critical)

Gateway outbox signs **JCS bytes**, not pretty JSON. This crate owns the algorithm depth:

| Behavior | Why it matters |
| --- | --- |
| Non-finite number guard | NaN/Inf must not enter signed preimage |
| UTF-16 code-unit key sort | Interop with non-BMP keys |
| ES6 / ryu number formatting | Stable float canonicalization |
| Strict duplicate-key reject (raw path) | Ambiguous JSON must not sign |
| Purity / conformance harnesses | Golden + property tests |

The conformance tests cover these fail-closed edges as well as the nominal RFC
8785 path.

---

## Directive fence corpus

- Golden cases embedded (`directive_fence_cases.json` / `fence_vectors`).
- Shared by the **council-rs emitter** and **gateway receiver** fences.
- Cases cover padded verbs that must not be trimmed, the Act tenant pin
  (`scope.tenant` must match the expected tenant), and rejection classes for
  `proposal.v1`.
- The authoritative cases are
  [`fence_vectors.rs`](../sovereign-protocol/src/fence_vectors.rs) and the
  [`fence_vectors_golden.rs`](../sovereign-protocol/tests/fence_vectors_golden.rs)
  cross-consumer test.

---

## Provenance and tokens

| Type | Role |
| --- | --- |
| `GatewayProvenance` | Gateway-side routing metadata (model/provider/fallback/request-id, captured from response headers — not cryptographic attestation) |
| `WorkerProvenanceGuard` / status | Worker completion; includes fabrication_guard posture |
| `ProviderProvenance` / `ProviderResponse` | Provider call receipts |
| `SeatResponse` | Council seat output shape (also referenced from council persistence docs) |
| `CapabilityToken` | prepare/execute sensitive path — field set must be documented for integrators |
| `ProblemDetails` | RFC 9457 problem+json errors |

---

## Escalation domain

The evidence and threshold language in `COMMS_CONTRACT.md` describes product
intent beyond the serialized fields. The wire schema is deliberately small:

- `SentinelState`: `tenant` (string), `sentinel` (string), `observed_at`
  (Unix epoch milliseconds), and arbitrary JSON `payload`.
- `Escalation`: `state` (`SentinelState`), `reason` (string), and `urgency`.
- `Urgency`: `low`, `medium`, or `high` on the wire.

The authoritative definitions are in
[`escalation.rs`](../sovereign-protocol/src/escalation.rs); consumers should not
invent additional fields.

---

## Tools

- `sentinel/tools/check_protocol_version_drift.py` — pin consumers to package version.
- `verify_provenance_harness.py` — placeholder stub (prints a banner and exits 0; no invariant checks yet).

## Implementation references

- Envelope and builder: [`lib.rs`](../sovereign-protocol/src/lib.rs)
- Directives: [`directive.rs`](../sovereign-protocol/src/directive.rs)
- Escalations: [`escalation.rs`](../sovereign-protocol/src/escalation.rs)
- Canonical JSON: [`jcs.rs`](../sovereign-protocol/src/jcs.rs)
- Shared types: [`types.rs`](../sovereign-protocol/src/types.rs)
- Wire and canonicalization tests: [`tests/`](../sovereign-protocol/tests)

---
