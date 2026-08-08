# Security Claims and Boundaries

This document describes the current code, not a roadmap or compliance claim.

| Claim | Status | Current boundary |
| --- | --- | --- |
| Product services are local by default | Enforced | Canonical runtime binds Council, Web, and Gateway to loopback. Loopback is still browser-reachable: a page on this machine can open Council WebSocket upgrades. The upgrade gate requires a present `Origin` to match the CORS allow predicate (any loopback origin plus the configured allow-list, which includes `tauri://localhost` by default). Absent `Origin` is allowed for non-browser local clients; a hostile external page carries a non-loopback Origin and is refused. Optional host CLI adapters bind all host interfaces for Docker Desktop bridge access, require generated bearer tokens, and must not be port-forwarded or exposed outside a trusted private boundary. |
| Private remote Web access | Optional | Installed IRIN.app Settings configures Tailscale Serve when the local client is installed and connected; it publishes to the operator's tailnet only, never to the public internet (Funnel is never configured by IRIN). Source development never applies or disables Serve. |
| Direct provider transport by default | Enforced | Council calls provider APIs and authenticated local CLIs directly unless a seat is explicitly set to Governed via Gateway. Exact CLI transport IDs remain host adapters: Gateway accepts only a fixed exact-adapter set and fails closed without a matching adapter — it does not silently rewrite an exact transport onto a different provider. Legacy class-ID policy under Governed mode is stated in the "Governed CLI routing" section below. |
| Installed DMG optional Gateway Pack | Enforced when used | Core DMG is Docker-free and Gateway-off by default. The optional app-owned pack (`irin-desktop-gateway`) requires Docker Desktop, digest-pinned images, Keychain-held `GW_API_KEY`, and authenticated `/v1/models` before governed proceedings. Watch producer/dispatcher stay false at boot, until the app's Touch ID ceremony arms the producer (the dispatcher is a separate env gate); authenticated Watch/Outbox reads are armed via a Keychain-held `WATCH_ADMIN_TOKEN` injected per-spawn by the native host (never written to disk, ambient values scrubbed); no host-home or gcloud mounts; Vertex remains Direct-only in v0.1. |
| Gateway caller authentication | Enforced | Missing or invalid caller credentials fail closed. |
| Discover credential handling | Enforced | Provider discovery reports detected/configured availability and provenance only; API-key environment variable names are returned, never values, and no discovery scan makes a billable inference call. A detected CLI binary is not proof of current authentication. |
| Signed Gateway audit ledger | Enforced on governed Gateway paths | Gateway signs routing, accounting, and decontaminator events into its tamper-evident audit ledger. This is distinct from the Watch fire chain and signed directive Outbox. |
| Ledger verify/export auth | Enforced | `GET /ledger/verify` and `GET /ledger/export` require admin-tier `X-Admin-Key` (not unauthenticated). |
| Watch as a bounded read surface | Enforced | War Room's Watch tab serves a capped, aggregated snapshot (registered Sentinels, recent fire counts) distinct from the full append-only `watch_fires` ledger. |
| Signed directives | Enforced on the Gateway outbox path | Gateway signs canonical directive bytes with the configured Ed25519 key. |
| Offline artifact verification | Enforced | Public-key verification of signed Outbox directives recomputes JCS over the stored envelope fields and verifies the signature. Arm confirm verifies the hardware ES256 signature over the **stored** stage challenge bytes (never a re-derived challenge). |
| Append-only watch record | Enforced by storage and tests | Watch fire records are hash-chained. SQLite `BEFORE UPDATE` / `BEFORE DELETE` triggers abort in-process SQL mutation, so mutation through the open database connection is detectable and refused. That is the tamper-evidence scope. The gap is filesystem replacement of the store: an attacker who can swap or rewrite `watch.db` on disk bypasses the chain and the triggers. The chain and triggers are not no-ops. |
| Spend limits | Enforced within configured Gateway paths | Governed call budget defaults (e.g. **$10/24h** per key) apply on those paths. Watch day-cap code ceiling is **$50/day** (`DAILY_SPEND_CAP_USD` may only lower it; raise or garbage refuses boot). Per-directive reservation code default is **$5** (`WATCH_MAX_FANOUT_COST_USD`). The canonical pack compose runs at **$25/day** with a **$2.50** fanout reserve. These are ex-ante reservations: claim reserves the ceiling up front, settle records realized cost and flags overshoot when realized exceeds the reservation — not a post-hoc hard clamp that can un-spend already accepted provider work. Outside Gateway, provider spend is unconstrained by IRIN. |
| Deterministic Sentinel decision | Enforced by the stock implementations | Sentinel observation and interest checks do not invoke an LLM. |
| Secret redaction | Partial | Known credential shapes are scrubbed on selected paths; arbitrary private content is not removed. |
| Action production | Disabled by default | Sentinel definitions, dispatcher (`WATCH_DISPATCHER_ENABLED`), producer startup, spend authorization, and the built-in worker loop (`WATCH_WORKER_ENABLED`) are separate gates. A hardware ceremony can start the producer and creates the signed `active_arm` required for spend. The boot triple-gate can start the producer but cannot authorize spend because it does not create `active_arm`. |
| Autonomous Worker execution | Not an operator feature (default off) | Product guidance ends at a signed Outbox directive. Authenticated claim, heartbeat, ack, worker-ack, and nack management routes are mounted. The built-in worker loop that uses them is disabled by default and is not an operator-ready autonomous executor. |
| Multi-tenant isolation | Not supported for public deployment | Current operation is single-operator and local-first. |
| Protection from host compromise | Not provided | A host-level attacker can read local data and replace software or credentials. |
| Compliance or certification | Not claimed | Controls may resemble external frameworks, but no attestation is made. |

## Governed CLI routing

Exact CLI transport IDs remain host adapters: Gateway has adapters only for a
fixed exact set and fails closed without a matching adapter — it does not
rewrite an exact CLI ID onto a different provider. The *legacy
class-substitution table* is what can still accept either a subscription-CLI
host adapter or the corresponding metered API route for historical seat names
(`claude` → `claude-cli` or `anthropic`, `gpt` → `gpt-cli` or `openai`, and
similar). Current policy **allows** that legacy substitution path so older
cabinets keep working under Governed mode. Do not read this as "governed mode
always routes CLI seats to the metered API" — exact CLI IDs stay on their host
adapters.

## Arm ceremony custody (wording)

Arm confirm is **single-operator principal token + local hardware
attestation**, not dual custody or four-eyes by two human tokens. One
`GW_ARM_PRINCIPALS` bearer stages and confirms; the second custody domain is
the enrolled Secure Enclave (se-p256) or FIDO2 key that signs the **stored**
stage challenge. User presence differs by credential type: for se-p256 the
protocol envelope carries no UP assertion and the product SE path is
biometry-gated (`gateway/bin/arm-attest.swift`); for fido2-es256 the envelope
must include `authenticatorData` and verification rejects it unless the
user-presence (UP) flag is set. Signature verification
at confirm (and later spend re-verify) is over those stored challenge bytes,
never a re-derived challenge.

Run `make verify` for the isolated signed-directive proof. Run the workspace and
component test targets for implementation coverage. A passing proof establishes
only the behavior it exercises.
