# Council ↔ Gateway Contract

This document is the contract between the gateway (this repository) and any
upstream caller — Council UI, the Librarian layer, future signed callers.

If you change anything in this file, you are changing the public surface
of the gateway. Bump `gateway:cache:vN:` if you change the cache key shape.

## Two-axis design

The gateway operates on **two axes** that serve different purposes:

**Axis 1 — Routing & Accounting (opinionless pipe).** The gateway
forwards model calls, translates between provider formats, accounts for
token usage and cost, caches responses, and tracks budgets. It has no
opinion on the meaning of a prompt. Anything domain-specific (sovereignty
scoring, sensitivity classification, council role semantics) belongs to
the caller.

**Axis 2 — Content Trust (structural rejection layer).** The gateway's
7-stage decontaminator inspects request payloads for structural attack
patterns — injection signatures, encoding attacks, prompt extraction
attempts — and rejects matching requests before they reach any provider.
This is a **reject-or-pass firewall**, not a semantic analyzer. The
decontaminator never mutates payloads, never interprets business meaning,
and never classifies content beyond structural pattern matching. It
operates at the same layer as a WAF rule: if the pattern matches, the
request is blocked; otherwise it passes through untouched.

The Ed25519 audit ledger spans both axes: it signs routing events (Axis 1)
and decontaminator verdicts (Axis 2) into the same tamper-evident chain.

---

## Versioning

| Field            | Value     |
|------------------|-----------|
| Contract version | 2         |
| Cache key prefix | `gateway:cache:v5:` (canonical: `CACHE_KEY_PREFIX` in `sidecar-rs/src/cache.rs`) |
| Trust root file  | `${LEDGER_SIGNING_KEY_PATH:-$HOME/.irin/ledger_key.pem}` |
| UDS path (in container) | `/run/sidecar/sidecar.sock` |

### Security posture — operator-aware surfaces

| Surface | Access | Rationale |
|---------|--------|-----------|
| `/metrics` (Prometheus) | Unauthenticated, localhost-only | Standard Prometheus scrape convention. Exposes counters and histograms (no PII, no keys). Acceptable for dev; production should front with a scrape proxy or restrict via IP policy. |
| `/admin/*`, `/auth/rotate` | Authenticated (admin_key in request body) | Proxied through nginx to sidecar. Auth enforced at sidecar level — bootstrap token or admin-tier virtual key required. |
| `/ledger/verify`, `/ledger/export` | Authenticated (admin-tier `X-Admin-Key`); loopback-oriented | Read-only ledger verification and export require an admin-tier virtual key via `X-Admin-Key` (401 if missing/invalid, 403 if non-admin). Exact-match routes only — `/ledger/record` is not exposed through nginx. Network binding remains loopback-oriented; auth is not optional. |
| UDS socket (`0660`) | Container-scoped | Socket permissions are owner+group read/write, world none; startup refuses an invalid mode. Both containers (gateway + sidecar) share a named volume and run as a shared group. In production with host-mounted sockets, keep `0660` with a shared group. |

---

## Layering

```
┌──────────────────────────────────────────────────────────────┐
│ Council UI / Librarian / other callers                       │
│  - classifies sensitivity                                    │
│  - assigns a council role to the call                        │
│  - decides routing strategy                                  │
│  - holds budget keys                                         │
└──────────────────────────────────────────────────────────────┘
                          │ HTTP (loopback 127.0.0.1) POST /v1/chat/completions (or /v1/messages, /v1/responses)
                          │ Headers: X-Sensitivity-Level, X-Council-Role,
                          │          X-Routing-Strategy, X-Budget-Key
                          ▼
┌──────────────────────────────────────────────────────────────┐
│ Gateway (this repo)                                          │
│  - guard / cache / route / budget / policy / translate       │
│  - signs every event into the audit ledger                   │
│  - returns provider-native or normalized response            │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
                 LLM provider (xAI, OpenAI, Anthropic, Vertex, NIM)
```

The gateway never reaches up the stack. It does not call its callers; callers
call it. There is no shared library between them — only this header
contract and the wire format below.

---

## Ingress headers

All headers are optional except for authentication. Missing values get conservative defaults.

### `Authorization: Bearer <key>` or `X-API-Key: <key>`

**Required**. The virtual API key used to authenticate the caller. The gateway validates this key against its local auth configuration and applies token-bucket rate limits (Global, per-IP, and per-key). The key's configuration can also securely override the `X-Budget-Key`.

### `X-Sensitivity-Level: GREEN | YELLOW | RED`

The caller's verdict on how sensitive this payload is. The gateway
**trusts this header** and acts on it without re-classifying the payload
(see "What the gateway does NOT do" below).

| Level    | Effect on routing |
|----------|-------------------|
| `GREEN`  | Default. Smart router scores all providers normally. |
| `YELLOW` | No automatic effect today; reserved for future per-provider tier filtering. Logged on the ledger. |
| `RED`    | **Forces routing to a local provider.** A requested `provider: local`/`mlx`/`ollama` model is already local; other requests resolve to `sovereign-node` (default Ollama). External cloud and CLI-pipe providers such as `claude-cli` are not eligible for the RED route. Logged on the ledger. |

Default when missing: `GREEN`.

### `X-Council-Role: <string>`

A free-form tag identifying which Council role is making this call
(`socrates`, `feynman`, `aurelius`, …, or `none`). The gateway records
this on every ledger event but takes no behavioral action on it. This is
how the caller correlates a multi-role deliberation in their own logs.

Default when missing: `none`.

### `X-Council-Transport-ID: <exact transport>`

Council uses this header to request one concrete transport, for example
`grok_api`, `claude_code`, or `codex_cli`. It is privileged metadata: Gateway
accepts it only when the authenticated key has `service_role=council` **and**
its key ID matches `COUNCIL_GATEWAY_KEY_ID`. The header is captured and stripped
at ingress, restored only into Gateway's internal request record after that
two-factor identity check, and is never forwarded upstream.
If any caller presents the header without that pinned identity, Gateway returns
`403 ERR_COUNCIL_TRANSPORT_IDENTITY`; it never strips the header and continues
through ordinary smart routing.

An accepted exact transport bypasses smart routing and model fallback. Gateway
must route to the adapter registered for that transport or fail closed. It does
not substitute a provider that happens to offer the same model. Canonical local
transports without Gateway adapters (`grok_build`, `grok_hermes`, and
`gemini_agy`) are rejected in Governed mode and remain available only for
Council's Direct mode. `X-Council-Original-Provider` is a migration-only alias
handled under the same trust and stripping rules.
If Sovereign mode is also active, the exact transport must map to a local
Gateway adapter or the request fails with
`403 ERR_COUNCIL_TRANSPORT_SOVEREIGN`; Gateway still never substitutes one.

### `X-Routing-Strategy: quality | balanced | economy | speed`

Hint to the smart router. Combined with task classification (coding /
creative / analysis / vision / tool-use / general) to pick the model.

Default when missing: `balanced`.

### `X-Budget-Key: <string>`

Per-tenant / per-project / per-experiment budget bucket. Used for
pre-flight cost check (deny if exceeded) and post-flight spend recording.

Default when missing: `default`.

### `X-Sovereign-Mode: true | false`

The "sovereign switch". When set to `true`, **all routing is forced to
local providers** regardless of declared sensitivity level. This is
functionally equivalent to every request being `RED`, but expressed as a
single toggle for operational convenience (e.g. private council runs).

Default when missing: `false`.

---

## Egress headers

The gateway adds these to its response when applicable.

| Header                   | Meaning |
|--------------------------|-------  |
| `X-Gateway-Request-ID`   | Gateway-owned request identifier for exact correlation with the ledger `request_id`. Present on every response, including `/v1` errors; never sourced from an upstream provider. |
| `X-Cache`                | `HIT` when the response was served from the L1/L2 cache. Absent on miss. |
| `X-Routed-Model`         | The model that actually ran after smart-routing and fallback resolution. Always set on non-cached responses. |
| `X-Routed-Provider`      | The provider that served the request (e.g. `nvidia`, `anthropic`). |
| `X-Routed-Fallback`      | `true` if the requested model was unavailable and a fallback was used. Absent otherwise. |
| `X-Requested-Model`      | The model alias the client originally requested (before routing resolution). |
| `X-Effective-Model`       | The concrete model ID after alias resolution and routing (same as `X-Routed-Model` but set in the header filter for downstream consumers). |
| `X-Sovereign-Mode`       | `active` when sovereign routing was engaged (either by `X-Sovereign-Mode: true` request header or `RED` sensitivity). |
| `X-DLP-Classification`   | DLP classification of the request payload when the DLP scanner is active. |
| `X-Ledger-Event-ID`      | (Future) The opaque ledger event ID for this call, for cross-system correlation. |
| `X-Batch-Op`             | The batch operation (`create` / `retrieve` / `cancel` / `list`) for `/v1/batches` requests. |
| `X-Idempotency-Replay`   | `true` when the response was served from the council idempotency store (matches a prior `Idempotency-Key` for an in-flight or stored council session). |
| `X-Council-Session-Id`   | Stable identifier for the council deliberation that produced this response. Joins seat-leaf rows, the chair `council_wrapper` row, and any subsequent `council_replay` rows in §6.4 aggregation. |
| `X-Total-Cost-Usd`       | Authoritative total cost for the council session this response belongs to (sum of leaf costs + `wrapper_cost_usd`). For non-council calls, absent or equal to per-call cost. |
| `X-Chair-Tokens`         | Output tokens consumed by the chair synthesis step alone. Useful for budgeting chair vs. seat token spend. |
| `X-RateLimit-Limit`      | The caller's burst-bucket capacity (requests per minute), echoed on both success and 429 responses for client backoff. |
| `X-RateLimit-Remaining`  | Tokens remaining in the caller's bucket at the time of the response. `0` on 429. |
| `X-RateLimit-Reset`      | Unix timestamp at which the caller's bucket will be fully refilled. |
| `X-Accel-Buffering`      | `no` when the response is a streaming (SSE) body, instructing nginx-derivative proxies upstream of the gateway to disable response buffering. |
| `X-Batch-Mode`           | `true` on Batch API responses, signalling the caller that token/cost accounting is deferred to the results download. |

These are best-effort and additive; absence is not an error.

---

## Wire format

The gateway is OpenAI-compatible at `/v1/chat/completions`, `/v1/responses`,
and `/v1/messages`. It accepts OpenAI-shape, Anthropic-shape, or
xAI/Responses-shape requests, and emits whichever shape the **target
provider** would have emitted (Anthropic and Vertex are translated back to
the caller's expected shape via the Rosetta layer).

Streaming is gated by `GW_ENABLE_STREAMING=1` (default off). When enabled,
Phase 2 supports SSE for all providers via a provider-aware SSE parser
(`lib/sse.lua`). Anthropic and Vertex streams are translated chunk-by-chunk
to OpenAI SSE shape in `body_filter_by_lua_block`. `claude-cli` returns `501`
(CLI pipe, not SSE). See the *Streaming (Phase 2)* section below for the
full contract.

---

## Cache key

```
SHA-256(alias || 0x00 || raw_body_bytes)
prefix:  see CACHE_KEY_PREFIX in sidecar-rs/src/cache.rs
         (currently "gateway:cache:v5:")
```

* `alias` is the **client-supplied** model name (e.g. `"opus"`),
  not the resolved provider model id (e.g. `"claude-opus-4-7"`).
  Two different aliases that resolve to the same model are cached
  separately. This is intentional — the alias is the cache identity.
* `raw_body_bytes` are the literal HTTP request body, not a re-encoded
  JSON form. Hashing the original bytes prevents drift between Lua
  `cjson.safe` and Rust `serde_json` canonicalization.
* The current prefix is `v5`; `CACHE_KEY_PREFIX` is authoritative. Entries
  written under older prefixes are unreachable and age out of TTL.

---

## Audit chain payload schema

Every request that parses past JSON validation produces a walkable chain
on the Ed25519-signed audit ledger. The chain is **one open-end event +
exactly one terminating event**, joined by `request_id`. Auditors walk
the chain by `request_id`; chain integrity is enforced by the SHA-256
hash chain over `(timestamp, source, target, payload, metadata,
schema_version, prev_hash)`.

### Open-end event (always fires once per accepted request)

| Field | Action | Decision | Payload |
|---|---|---|---|
| `request_received` | (no decision) | `{ request_id, raw_body_sha256, raw_body_size_bytes, message_count }` |

`raw_body_sha256` is a SHA-256 fingerprint of the literal HTTP request
bytes. It is a **one-way hash, not content** — the gateway has no
opinion on the body, but signs a fingerprint of what was received for
non-repudiation.

### Terminating events (exactly one fires per accepted request)

| Action | Decision | Payload includes |
|---|---|---|
| `guard_input` | `blocked` | `{ request_id }` |
| `cache_check` | `hit` | `{ request_id, response_body_sha256, response_size_bytes }` |
| `route_decide` | `rejected` | `{ request_id }` (e.g., unknown model) |
| `budget_check` | `blocked` | `{ request_id, budget_key }` |
| `policy_evaluate` | `blocked` | `{ request_id, provider, level }` |
| `outbound_response` | (no decision) | `{ request_id, tokens_in, tokens_out, cached_in, cost_usd, latency_ms, status, response_body_sha256, response_size_bytes, kind? }` |
| `outbound_batch` | (no decision) | `{ request_id, batch_op, status, response_body_sha256, response_size_bytes }` |
| `council_replay` | `replay` | `{ request_id, kind: "council_replay", council_session_id, idempotency_key, raw_body_sha256, response_body_sha256, original_request_id, wrapper_cost_usd, latency_ms, status }` |

The `batch_received` action is the open-end equivalent for `/v1/batches`
requests (mirrors `request_received`).

`response_body_sha256` hashes the **post-translate wire bytes** — what
the client actually received, not the upstream native body. This is
the load-bearing field for non-repudiation: paired with
`raw_body_sha256` from the open-end event, an auditor can prove
"request X produced response Y" without the gateway storing either body.

### `kind` enum (§6.4 aggregation discriminator)

On terminator payloads, `kind` discriminates rows that participate in
council aggregation:

| `kind`             | Where set                                                    | Aggregation semantics |
|--------------------|--------------------------------------------------------------|------------------------|
| `leaf`             | seat-level calls inside a council deliberation                | counted as direct cost in `SUM(cost_usd)` for the council session; excluded from `wrapper_cost_usd` to avoid double-counting |
| `council_wrapper`  | the synthesizing chair call that returns the council session | carries `wrapper_cost_usd` (chair-only), `chair_tokens`, `council_session_id` — represents the synthesis layer in §6.4 totals |
| `council_replay`   | idempotent replay of a previously stored council response    | contributes **zero** to `wrapper_cost_usd` aggregation; references the original via `original_request_id`; both digests (`raw_body_sha256`, `response_body_sha256`) match the stored row for non-repudiation |
| (unset)            | non-council direct routing                                    | normal single-row accounting; aggregation ignores rows without `kind` |

§6.4 invariant: for any `council_session_id`, the sum across leaf rows
plus the wrapper row's `wrapper_cost_usd` equals the total cost reported
to the client via `X-Total-Cost-Usd`. Replay rows do not contribute to
this sum — the original wrapper row owns the cost, and the replay row
provides the audit trail proving "same idempotency key, same response
bytes."

The canonical SQL for this aggregation lives at
[`test/sql/council_644_aggregation.sql`](test/sql/council_644_aggregation.sql)
and is exercised from the IRIN root by `make -C gateway test-council-644`
against a synthetic session
(3 leaf + 1 wrapper + 1 replay rows). Any change to the `kind` enum or
payload field names must update the SQL and keep the test green.

### Reliability

All ledger writes use the bounded retry helper at `lua/lib/ledger.lua`:
3 attempts at 50ms / 200ms / 500ms backoff. A failure to commit is
logged via `ngx.ERR` as `event=ledger_commit_failed` so silent chain
degradation is observable.

### Schema versioning

Every ledger event carries a `schema_version` column (INTEGER, currently `3`).

Schema history:
- **v1:** 7-field pipe-delimited preimage (`timestamp|source|...|schema_version|prev_hash`)
- **v2:** 8-field pipe-delimited (`...caller_key|prev_hash`) — no production events
- **v3:** length-prefixed encoding (`{len}:{value}|...`) — eliminates delimiter-collision risk

The version is injected server-side by the Rust sidecar in `record_event()`.
Callers (Lua, future external writers) do not set it. The constant
`LEDGER_SCHEMA_VERSION` in `sidecar-rs/src/ledger.rs` is the single source of
truth.

Additional columns (schema v2+):
- `caller_key` (TEXT, nullable) — resolved `key_id` from auth, included in hash preimage
- `signing_key_pubkey` (TEXT, nullable) — hex pubkey that signed the event (verifier index, NOT in hash preimage)

On startup, the sidecar validates that the `audit_events` table has the
`schema_version` column. If the column is missing (old DB), the sidecar panics
with a clear message directing the operator to delete the DB and restart.

### Key lifecycle events

Two ceremony event targets are defined for key rotation:

| Target | Payload fields |
|--------|---------------|
| `key_introduce` | `new_pubkey_hex`, `purpose` (enum: `ledger_signing`), `introduced_by_pubkey_hex`, `envelope_signature_hex` |
| `key_revoke` | `revoked_pubkey_hex`, `reason`, `revoked_by_pubkey_hex`, `envelope_signature_hex` |

Each carries a domain-separated envelope signature (`GW-INTRODUCE-v1` / `GW-REVOKE-v1` tag + length-prefixed fields). The `gateway-ledger fsck` command verifies both the chain signature and the envelope signature, cross-checking `introduced_by_pubkey_hex` against the row's `signing_key_pubkey`.

### Provider cache optimization

The gateway automatically injects provider-specific cache hints to maximize
prompt prefix caching:

- **Anthropic:** The translator restructures the `system` field into a
  content-block array and injects `cache_control: {type: "ephemeral"}` on
  the final block. If the caller already provides content blocks with their
  own `cache_control` markers, the gateway passes them through untouched.
  Callers who need cache breakpoints on different blocks must send the full
  structured system array with their own markers.

- **xAI:** The `x-grok-conv-id` header is set to the caller's budget key
  (from `X-Budget-Key`) for distributed cluster cache affinity. Not injected
  when the budget key is missing or "default".

- **OpenAI:** The `prompt_cache_key` field is set in the request body to
  the caller's budget key for prefix cache grouping. Not injected when the
  budget key is missing or "default".

Cache optimization is always-on and requires no caller opt-in. Budget keys
used for affinity hints are truncated to 128 characters and stripped of
control characters.

### What the audit chain does NOT carry today

* No raw prompt content. `raw_body_sha256` is a fingerprint only.
* No redacted-prompt-hash. Council Tier-1 follow-up may add a redacted
  fingerprint *alongside* the raw fingerprint — both verifiable, neither
  a content opinion. Until that lands, the chain answers "did this exact
  request happen?" but not "was the prompt sanitized first?"
* No cross-references to upstream provider request IDs. End-to-end
  reconstruction across the gateway → upstream API boundary requires
  log-correlation by timestamp.

### Configurable decontaminator stages

The 7-stage input decontaminator is configurable at startup via a JSON
file (path set by `DECON_CONFIG_PATH` env var, default: no file → hardcoded
defaults). Each stage can be individually controlled:

```json
{
  "block_severity": 0.85,
  "max_threats_before_block": 5,
  "max_payload_len": 1000000,
  "dry_run": false,
  "stages": {
    "oversized_payload": { "enabled": true, "mode": "reject" },
    "encoding_attack":   { "enabled": true, "mode": "reject" },
    "ghost_gate":        { "enabled": true, "mode": "reject" },
    "malformed_content": { "enabled": true, "mode": "reject" },
    "zero_width":        { "enabled": true, "mode": "reject" },
    "homoglyph":         { "enabled": true, "mode": "reject" },
    "prompt_injection":  { "enabled": true, "mode": "reject" }
  }
}
```

| Mode | Effect |
|------|--------|
| `reject` | Detections count toward `block_severity` and `max_threats_before_block` thresholds. |
| `log_only` | Detections are recorded (observability / metrics) but severity is zeroed — they never trigger a block. |

Setting `enabled: false` skips the stage entirely (no detection, no metrics).
Unspecified stages default to `{ "enabled": true, "mode": "reject" }`.
The `GUARD_DRY_RUN=1` env var overrides the file's `dry_run` field.

### Ledger verification CLI

Standalone binary (`src/bin/gateway_ledger.rs`), three subcommands:

**`gateway-ledger verify <db-path> [--key <path>]`** — hash chain + signature check.

**`gateway-ledger fsck <db-path> [--key <path>]`** — full semantic check:
1. Hash chain integrity + signature validity
2. Schema version monotonicity (versions never decrease)
3. `signing_key_pubkey` presence on v3+ events
4. Key lifecycle scanning (introduce/revoke detection)
5. Envelope signature verification with signer cross-check
6. Revoked-key usage detection, duplicate introduce detection

**`gateway-ledger generate-key <output-path>`** — generates a 32-byte Ed25519 seed file (chmod 600).

Exit codes: `0` = valid/healthy, `1` = tampered/unhealthy, `2` = usage/IO error.

---

## Trust root

The Rust sidecar signs every ledger event with an Ed25519 signing key
loaded once at startup from disk.

```
default path:  $HOME/.irin/ledger_key.pem
override:      LEDGER_SIGNING_KEY_PATH
required size: exactly 32 bytes
required mode: 0600
```

**The `.pem` extension is misleading.** The file is **32 raw bytes**, not
a PEM-encoded key. The Rust sidecar passes those bytes directly to
`ed25519-dalek::SigningKey::from_bytes`.

The sidecar **fails closed** at startup if:
* the file is missing,
* the file is not exactly 32 bytes,
* the mode is not `0600`.

`LEDGER_OLD_SIGNING_KEY_PATH` receives the same strict validation when set.

There is **no ephemeral key generation**. Generating a fresh key on
missing-file would silently invalidate every previously-signed event in
the audit chain. The startup panic is the correct behavior.

**Watch / ledger provenance asymmetry:** `watch_fires` is hash-chained but not
individually signed. Ledger audit events and directive outbox rows are
Ed25519-signed. Key ceremony events and `fsck` own key lifecycle verification.

**Key rotation:** `POST /auth/rotate` (admin-only) generates a new keypair,
writes a `key_introduce` ceremony event signed by the current active key,
and stages the new key to `LEDGER_NEW_KEY_STAGING_PATH` (default
`/run/sidecar/new_ledger_key.bin`, 0600). The operator inspects, deploys,
sets `LEDGER_OLD_SIGNING_KEY_PATH` to the previous key, and restarts.
The old key remains accepted for verification during the dual-signing window.

---

## What the gateway does NOT do

These are **the caller's concerns**, not the gateway's. The gateway will
trust an upstream verdict (header) but will never compute one itself.

* **Sensitivity classification.** No DLP, no PII detector, no
  regex-on-the-payload. The caller passes `X-Sensitivity-Level`.
* **Sovereignty / values alignment scoring.** The `/guard/sovereignty`
  endpoint exists for callers that want it, but the gateway's own
  outbound flow does not invoke it and does not record a sovereignty
  score on ledger events.
* **Council role semantics.** `X-Council-Role` is a passthrough tag.
  The gateway never branches on it.
* **Semantic content analysis.** The decontaminator rejects structural
  attack patterns (Axis 2) but never interprets business meaning, scores
  content quality, or classifies topics. MCP tool-call schemas are
  treated as opaque bytes — no semantic parsing.

If you need any of the above, do it in the caller before calling the
gateway, and pass the verdict in via the headers above.

### Streaming (Phase 2)

SSE streaming is gated by `GW_ENABLE_STREAMING=1` (default off). When
enabled, all providers are supported via a stateful SSE frame parser
(`lua/lib/sse.lua`). The parser handles named events (Anthropic), plain
data events (OpenAI/Vertex), CRLF normalization, SSE comments, and TCP
fragmentation.

Provider streaming behavior:

| Provider    | SSE Shape             | Translation     | Usage Extraction |
|-------------|-----------------------|-----------------|------------------|
| `openai`    | OpenAI `data:` chunks | Passthrough     | Final-chunk `usage` |
| `xai`       | OpenAI `data:` chunks | Passthrough     | Final-chunk `usage` |
| `nvidia`    | OpenAI `data:` chunks | Passthrough     | Byte-length estimate (unverified) |
| `anthropic` | Named events          | Chunk-by-chunk → OpenAI | `message_start` input + `message_delta` output |
| `vertex`    | `streamGenerateContent?alt=sse` | Chunk-by-chunk → OpenAI | `usageMetadata` (last wins) |
| `local`     | OpenAI `data:` chunks | Passthrough     | Final-chunk `usage` |
| `claude-cli`| N/A — returns `501`   | N/A             | N/A |

Anthropic-specific: tool-use `input_json_delta` fragments are buffered
and emitted as a complete OpenAI `tool_calls` chunk at `content_block_stop`.
Thinking blocks (`thinking_delta`) are silently skipped.

Vertex-specific: the path is swapped from `:generateContent` to
`:streamGenerateContent?alt=sse` at routing time.

Streaming responses skip cache (both check and store). The `is_streaming`
flag appears in audit log entries and ledger metadata (additive — no
schema break).

---

## Body size and memory profile

The gateway enforces a **2MB hard cap** on request bodies via
`client_max_body_size 2m` in `nginx.conf`. Oversize requests are rejected
at the nginx layer with `413 Request Entity Too Large` before any Lua
code runs. This bound is load-bearing for memory:

* `raw_body` is captured on `ngx.ctx.gw.record.raw_body` for the full
  request lifetime (cache_store + inbound-ledger sha256 closures both
  capture it).
* At 2MB × 100 concurrent requests, worker heap stays under 200MB.
* No spill-to-scratch-file mechanism exists. The single-host deployment
  model (see *Threat model* below) does not require one.

Implications for callers:
* Vision payloads must keep base64-encoded images under ~1.5MB after
  JSON encoding (a 1024×1024 JPEG fits comfortably).
* Long conversation histories should be trimmed by the caller, not the
  gateway. The gateway has no opinion on what content is "important."
* If a future use case needs >2MB requests, raise the cap explicitly,
  re-profile worker memory at expected concurrency, and only THEN
  consider spill-to-disk. Do not raise the cap silently.

---

## Threat model

**Today: authenticated proxy, localhost-only, single-tenant.** The gateway requires API key authentication (`Authorization: Bearer <key>`) backed by token-bucket rate limits and budget enforcement, but it binds to `127.0.0.1` and is **not designed for direct network exposure**. Multi-tenant isolation is not shipped.

**v3 (future): network deployment requires mTLS.**
Before exposing the gateway beyond internal boundaries, you must add:
* a TLS listener with client certificate verification, and
* signed ingress headers (so callers cannot spoof
  `X-Sensitivity-Level: GREEN` to bypass RED routing).

---

## Compatibility & deprecation

This contract is versioned (`Contract version` at the top). Any change
to header semantics, cache key shape, or trust root format is a breaking
change; bump the version and document the migration here.

Non-breaking additions (new optional headers, new ledger metadata fields)
do not bump the version.

### Changelog

**v2:** Council-as-Gateway endpoint.
- Adds terminator actions `outbound_batch`, `batch_received`, and `council_replay`.
- Adds the `kind` enum (`leaf` | `council_wrapper` | `council_replay`) on terminator payloads, with §6.4 aggregation semantics specified.
- Adds egress headers `X-Idempotency-Replay`, `X-Council-Session-Id`, `X-Total-Cost-Usd`, `X-Chair-Tokens`, `X-Batch-Op`.
- `make -C gateway contract-check` now fails the build if any action name
  appearing in a `ledger_record` / `ledger_schedule` call site in `lua/` is
  missing from the terminator or open-end tables above.

**v1:** Initial contract — single-tenant audit ledger + decontaminator + cache key prefix (canonical value in `sidecar-rs/src/cache.rs`; bumped on cache-shape change).
