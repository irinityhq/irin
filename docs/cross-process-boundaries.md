# Cross-process boundaries

Inventory of IRIN's process and language boundaries: who talks to whom, what
the wire looks like, where the typed contract lives, and what is still
stringly typed. Use this when changing headers, Lua→sidecar routes, spawn env,
or envelope shapes.

This is not an OpenAPI specification and not authorization to arm Watch.

## What already has a typed IDL

**`sentinel/sovereign-protocol`** is the sole envelope wire-format crate for
Escalation / Directive, outer `{"v":1,"envelope":…}` wrapping, JCS (RFC 8785)
signing preimages, provenance types, and capability tokens. Intent SSOT:

- [`sentinel/COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md)
- [`sentinel/docs/protocol-implementation.md`](../sentinel/docs/protocol-implementation.md)

Council and Gateway both depend on that crate. **Do not add a second envelope
IDL.** Gaps below are boundaries *outside* that crate — especially Lua and
env/spawn contracts that cannot consume Rust types.

## Boundary table

| ID | Boundary | Ends | Wire | Doc SSOT | Gap | Done-criteria for enforcement |
| --- | --- | --- | --- | --- | --- | --- |
| **B1** | OpenResty Lua ↔ sidecar-rs | `gateway/lua/` (`sidecar.lua`, `router.lua`) ↔ `gateway/sidecar-rs` Axum over UDS | HTTP/JSON on the sidecar socket (`SIDECAR_ADDR`; defaults documented in the Gateway contract). Lua tables via `cjson` — **untyped** | Partial: [`gateway/docs/gateway-core-surfaces.md`](../gateway/docs/gateway-core-surfaces.md), route comments in `sidecar-rs`, [`surface-map.md`](surface-map.md). No shared schema file | Largest hole: Lua body keys can drift from Rust handlers with no static check | (1) Lua `sidecar_post` paths match Rust mounts. (2) Hot paths pin required keys (`/auth/check`, `/guard/input`, budget/route). (3) New Lua path without a sidecar route is a finding. Rules pin strings + keys only — no second IDL |
| **B2** | Council ↔ Gateway (governed) | `council-rs/src/provider/gateway.rs` → OpenResty `:18080` → Lua → sidecar | Loopback HTTP to `/v1/chat/completions` plus Council headers (`Authorization`, `X-Council-Depth`, `X-Council-Transport-ID`, `X-Council-Original-Provider`, `X-Sensitivity-Level`, `X-Council-Request-ID`, optional sovereign/parent ids) | **[`gateway/COUNCIL_GATEWAY_CONTRACT.md`](../gateway/COUNCIL_GATEWAY_CONTRACT.md)** | Header set is doc+code SSOT but not rule-checked for drift | (1) Required headers present on the governed POST. (2) Doc header names match caller emit (or intentional absence is noted) |
| **B3** | War Room web ↔ Council | `council-rs/warroom/web/` ↔ `council-rs/src/server/*` | REST JSON + WebSocket (defaults `http://127.0.0.1:8765` / `ws://…`) | [`architecture.md`](architecture.md); route inventory is code-first | Strongest auto-extracted contract surface (HTTP/WS); body shapes not validated by extraction | Client+server pairs for critical routes; body rules only if a concrete drift class appears |
| **B4** | warroom-tauri ↔ council spawn | `council-rs/warroom-tauri/src-tauri/src/sidecar.rs` → `council --serve` | **Env contract** (not HTTP): CORS, auth token / dev no-auth, `COUNCIL_VIA_GATEWAY`, scrub/re-inject of gateway keys, optional `LIBRARIAN_BASE_URL` | Rustdoc on `sidecar.rs`; warroom-tauri README; `warroom/docs/TAURI-AUTH.md` | Scrub-key list is stringly; inheritance leak if incomplete | Scrub list complete before governed re-inject; unit tests remain behavior SSOT |
| **B5** | Outbox / Watch / envelopes | Gateway watch + outbox; types in **sovereign-protocol**; council also consumes the crate | Signed envelopes + `/watch/outbox/*` HTTP; Lua proxies many watch routes **opaquely** | [`COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md), surface-map Watch section | Not “missing IDL” — non-Rust peers and HTTP surface drift | Keep sovereign-protocol tests + formal JCS checks; no new envelope IDL; HTTP allowlists at the Lua edge only if needed |

## Priority for static enforcement

When adding Opengrep or similar path/key rules, prefer this order:

1. **B1** — Lua↔sidecar path and required JSON keys (highest untyped surface).
2. **B4** — tauri spawn env scrub / governed re-inject.
3. **B2** — Council→Gateway required headers in `gateway.rs`.

B3 stays on e2e + extracted contracts unless a body-schema gap shows up. B5
stays on sovereign-protocol + existing formal/scanner coverage.

## Covered vs not (sovereign-protocol)

| Covered (typed Rust + tests) | Not covered by sovereign-protocol |
| --- | --- |
| Escalation / Directive envelopes, CE reject paths | **B1** Lua↔sidecar internal JSON |
| JCS + selective formal proofs | **B2** provider HTTP headers / OpenAI-shape bodies |
| Fence golden vectors (council ↔ gateway) | **B3** War Room TS REST/WS JSON |
| Provenance / CapabilityToken / ProblemDetails | **B4** Tauri→council env spawn |
| Cargo consumers: `gateway-sidecar`, `council-rs` | Lua proxies that forward watch/outbox without parsing envelopes |

## Related

- [`architecture.md`](architecture.md) — Direct vs Governed product shape
- [`surface-map.md`](surface-map.md) — operator-visible HTTP/Watch surfaces
- [`security-tooling.md`](security-tooling.md) — scanners, Selene, local Gortex layers
- [`gateway/COUNCIL_GATEWAY_CONTRACT.md`](../gateway/COUNCIL_GATEWAY_CONTRACT.md)
- [`sentinel/COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md)
