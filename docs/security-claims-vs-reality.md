# Security Claims and Boundaries

This document describes the current code, not a roadmap or compliance claim.

| Claim | Status | Current boundary |
| --- | --- | --- |
| Product services are local by default | Enforced | Canonical runtime binds Council, Web, and Gateway to loopback. Optional host CLI adapters bind all host interfaces for Docker Desktop bridge access, require generated bearer tokens, and must not be port-forwarded or exposed outside a trusted private boundary. |
| Private remote Web access | Optional | Tailscale Serve is configured only when the local client is installed and connected; it publishes to the operator's tailnet only, never to the public internet (Funnel is never configured by IRIN). |
| Direct provider transport by default | Enforced | Council calls provider APIs and authenticated local CLIs directly unless a seat is explicitly set to Governed via Gateway; Gateway never silently substitutes a provider for an exact transport it has no adapter for. |
| Installed DMG optional Gateway Pack | Enforced when used | Core DMG is Docker-free and Gateway-off by default. The optional app-owned pack (`irin-desktop-gateway`) requires Docker Desktop, digest-pinned images, Keychain-held `GW_API_KEY`, and authenticated `/v1/models` before governed proceedings. Watch producer/dispatcher stay false; no host-home or gcloud mounts; Vertex remains Direct-only in v0.1. |
| Gateway caller authentication | Enforced | Missing or invalid caller credentials fail closed. |
| Discover credential handling | Enforced | Provider discovery reports detected/configured availability and provenance only; API-key environment variable names are returned, never values, and no discovery scan makes a billable inference call. A detected CLI binary is not proof of current authentication. |
| Signed Gateway audit ledger | Enforced on governed Gateway paths | Gateway signs routing, accounting, and decontaminator events into its tamper-evident audit ledger. This is distinct from the Watch fire chain and signed directive Outbox. |
| Ledger verify/export auth | Enforced | `GET /ledger/verify` and `GET /ledger/export` require admin-tier `X-Admin-Key` (not unauthenticated). |
| Watch as a bounded read surface | Enforced | War Room's Watch tab serves a capped, aggregated snapshot (registered Sentinels, recent fire counts) distinct from the full append-only `watch_fires` ledger. |
| Signed directives | Enforced on the Gateway outbox path | Gateway signs canonical directive bytes with the configured Ed25519 key. |
| Offline artifact verification | Enforced | Public-key verification recomputes canonical bytes and verifies signatures. |
| Append-only watch record | Enforced by storage and tests | Watch fire records are chained; mutation is detectable. |
| Spend limits | Enforced within configured Gateway paths | Governed call budget defaults (e.g. **$10/24h** per key) and Watch producer ceilings (**$50/day** hard max, env may only lower — the canonical runtime runs at **$25/day** with a **$2.50** fanout reserve) apply only on those paths. Outside Gateway, provider spend is unconstrained by IRIN. |
| Deterministic Sentinel decision | Enforced by the stock implementations | Sentinel observation and interest checks do not invoke an LLM. |
| Secret redaction | Partial | Known credential shapes are scrubbed on selected paths; arbitrary private content is not removed. |
| Action production | Disabled by default | Sentinel definitions, dispatcher (`WATCH_DISPATCHER_ENABLED`), producer startup, spend authorization, and the built-in worker loop (`WATCH_WORKER_ENABLED`) are separate gates. A hardware ceremony can start the producer and creates the signed `active_arm` required for spend. The boot triple-gate can start the producer but cannot authorize spend because it does not create `active_arm`. |
| Autonomous Worker execution | Not an operator feature (default off) | Product guidance ends at a signed Outbox directive. Authenticated claim, heartbeat, ack, worker-ack, and nack management routes are mounted. The built-in worker loop that uses them is disabled by default and is not an operator-ready autonomous executor. |
| Multi-tenant isolation | Not supported for public deployment | Current operation is single-operator and local-first. |
| Protection from host compromise | Not provided | A host-level attacker can read local data and replace software or credentials. |
| Compliance or certification | Not claimed | Controls may resemble external frameworks, but no attestation is made. |

Run `make verify` for the isolated signed-directive proof. Run the workspace and
component test targets for implementation coverage. A passing proof establishes
only the behavior it exercises.
