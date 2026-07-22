# Architecture

IRIN is a local-first, single-operator product. This document maps how War
Room, Council, Direct provider transport, Gateway, and the optional Sentinel
lane fit together, and separates artifacts that are easy to conflate:
source receipts, precedent `RetrievalReceipt`s, the signed Gateway audit
ledger, Watch, and signed Outbox directives.

## Product shape

```text
Browser or Tauri desktop shell (War Room)
    | REST + WebSocket, http://127.0.0.1:3010 -> :8765
    v
Council (deliberation engine, :8765)
    | per-seat calls through each configured provider transport
    |
    +--> Direct provider transport (default)
    |       straight to the provider API or authenticated local CLI
    |
    +--> Gateway (:18080, explicit opt-in per seat)
            metering, budget, provenance, then the same provider
```

War Room is the operator surface and the product face: it renders
deliberation rounds, direct-fire prompts, session history, provider
discovery, cabinet editing, the Gateway Outbox, the Watch view, and drift
analysis. It talks only to Council's REST/WebSocket API; it never calls a
provider directly.

Council is the deliberation engine. For each seat it resolves one exact
transport (for example `claude_code`, `grok_api`, `nvidia`) and calls it
either **Direct** or **Governed via Gateway**.

- **Direct is the default.** Council calls the provider API or an
  authenticated local CLI itself, using credentials inherited from the
  operator's login shell.
- **Gateway is explicit opt-in governance for supported routes.** Selecting
  "Governed via Gateway" for a seat, or setting `COUNCIL_VIA_GATEWAY=1`,
  routes that seat's calls through Gateway instead, which adds metering,
  budget enforcement, and a signed audit-ledger record of the call. Gateway
  is not a maturity ladder and never silently substitutes providers: it has
  adapters for a fixed set of exact transports (`grok_api`, `claude_api`,
  `claude_code`, `openai_api`, `codex_cli`, `gemini_vertex`, `gemini_cli`,
  and `nvidia`/`nim`). A cabinet that references a transport without a
  Gateway adapter — `grok_build`, `grok_hermes`, or `gemini_agy` — is
  rejected in Governed mode and stays Direct-only. See
  [`council-rs/docs/providers.md`](../council-rs/docs/providers.md) for the
  full transport matrix.

Gateway itself is a two-part local service: OpenResty at `:18080` accepts
caller requests and talks to a Rust sidecar over a Unix domain socket. The
sidecar owns caller authentication (fail-closed), budget checks, the watch
plane, the signed directive outbox, and arming controls. See
[`gateway/COUNCIL_GATEWAY_CONTRACT.md`](../gateway/COUNCIL_GATEWAY_CONTRACT.md)
for the exact header and wire contract between Council and Gateway.

For a compact inventory of HTTP surfaces, Sentinels, arming, and protocol
modules, see [`surface-map.md`](surface-map.md). Watch operator detail:
[`../gateway/docs/watch-api.md`](../gateway/docs/watch-api.md).

## Evidence and claim validation (Sheldon)

Sheldon is the between-round claim validator: after a round of model
responses, it checks factual claims made in that round and returns a verdict
per claim — supported, consistent, or no-evidence — before the next round or
the chair ruling. This runs inside ordinary Council deliberation; it is not a
Gateway or Sentinel path.

When validation is enabled, Sheldon gathers bounded evidence before the
validator model runs, in this order:

1. **Provider evidence.** Exa, Tavily and Tavily News, Firecrawl for cited
   URLs, and optional Semantic Scholar. This is the primary path and needs no
   xmcp instance.
2. **Live X posts (optional, XMCP-only).** If a local xmcp MCP instance is
   reachable, Sheldon calls only its `searchPostsRecent` tool
   (`council-rs/src/xmcp.rs`) for recent X posts. Sheldon does not consult a
   personal bookmark or intel corpus through xmcp. xmcp is optional and not
   required for IRIN to run; if it is down, X evidence from that path is
   simply absent, not an error.
3. **Direct Grok fallback.** If the gather above returns no evidence, Council
   falls through to the `grok-cli-default` Grok Build seat, which keeps its
   own native web and X search directly against the provider.

Gateway does not provide native web or X search: governed routes must not
claim tools the Gateway cannot preserve. Operator detail, including the
model pin and fallback order: [`council-rs/docs/providers.md`](../council-rs/docs/providers.md).

## Discover: the safe credential front door

Discover (`GET /api/discover`) reports which provider transports are detected
or configured — API-key variable present, supported local CLI binary present,
local model runtime reachable — without making a billable inference call and
without ever returning a credential value to the browser. It combines:

- a compiled catalog of known transports, including unavailable ones so the
  operator can see what could be enabled;
- presence of non-empty API-key environment variables inherited from the
  login shell (names only, never values);
- detected supported local CLI/adapter binaries (presence is not proof that
  the CLI's login is still valid);
- local Ollama, LM Studio, and LocalAI endpoints;
- an optional `~/.config/council/providers.toml` compatibility file for
  custom transport entries — never a required step, and never a place IRIN
  copies a key into; and
- live model-catalog requests for supported configured APIs and local
  runtimes.

Discover also reports, per transport, whether Gateway has an adapter for it.
An installed transport can be available for Direct mode but marked "Direct
only" if Gateway has no adapter for it.

## Source receipts, RetrievalReceipts, audit ledger, Watch, and Outbox

These five artifacts are easy to conflate and are architecturally distinct:

- **Source receipt.** Written by the runtime controller
  (`scripts/irin-runtime.sh`) to `~/.local/state/irin/runtime/source.json`
  each time the canonical runtime starts. It records the exact Git origin,
  branch, commit SHA, and tree cleanliness of the checkout that owns the
  running services, so `make runtime-status` can detect drift between the
  checkout and what is actually running.
- **Precedent `RetrievalReceipt`.** A frozen, in-process result of one
  precedent-index retrieval (`council-rs/src/precedent/mod.rs`), carrying
  the ranker identity (`hybrid-v1` or `keyword-v1`), the query, and each
  ranked hit's session id, score, and why-matched explanation. The same
  receipt feeds prompt injection, the streamed `precedent_loaded` event, and
  the persisted `session.precedent_ids` field, so all three stay identical
  by construction for one deliberation run.
- **Signed Gateway audit ledger.** The governed Gateway path writes routing,
  accounting, and decontaminator events into its Ed25519-signed,
  tamper-evident ledger. This is the provider-call provenance surface. It is
  not the Watch fire chain and it is not the directive Outbox.
- **Watch.** A bounded, sanitized read surface over the Gateway watch plane
  — registered Sentinels, recent fire counts, and a capped list of recent
  fires — served to War Room's Watch tab. It is not the underlying
  append-only ledger: the full `watch_fires` table is a hash-chained,
  unbounded-by-design log (verifiable with `GET
  /watch/verify-chain/{tenant}`), while the Watch UI snapshot only exposes a
  recent, aggregated view of it. An empty or quiet Watch view does not mean
  Council or Gateway is unhealthy.
- **Signed Outbox directive.** A `directive_outbox` row, Ed25519-signed by
  Gateway's sidecar over canonical JSON (RFC 8785 JCS) bytes, produced only
  by the Sentinel-to-Watch-to-Council-triage-to-producer lane described
  below. Offline verification recomputes the canonical bytes and checks the
  signature; it does not run through ordinary Council deliberation.

## Ordinary deliberation vs. the Sentinel/Outbox lane

Running a Council deliberation from the CLI or War Room — Deliberate, Direct
Fire, any cabinet — never creates a signed Outbox directive. That artifact
belongs to a separately enabled lane:

```text
Sentinel observes evidence -> Gateway watch plane records and routes
    -> Council deliberates on the escalation -> Gateway validates and signs
    a directive into the outbox
```

Sentinels are deterministic: they observe state and decide whether evidence
is interesting without calling an LLM
([`sentinel/COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md)). Gateway's
stock Sentinel implementations live under
`gateway/sidecar-rs/src/watch/sentinels/` (file inbox, silence, queue depth,
watch health, ledger delta, anomaly, completion verification, precedent
integrity). The canonical local runtime loads a committed test profile with
exactly one deterministic file-inbox watch, and the watch producer and
dispatcher are disabled by default (`WATCH_DISPATCHER_ENABLED=false`,
`WATCH_PRODUCER_ENABLED=false`). Enabling a Sentinel definition does not
enable the producer, and enabling the producer does not arm an action path
— those are three independent gates. Arming requires an explicit,
hardware-backed operator ceremony; see
[`gateway/docs/runbooks/arming-authorization.md`](../gateway/docs/runbooks/arming-authorization.md).

The supported product path ends at a signed directive artifact. Authenticated
claim, heartbeat, acknowledgement, worker-acknowledgement, and negative-
acknowledgement routes are mounted as management surfaces. The built-in worker
loop that uses them is disabled by default and is not operator-ready.

## Optional private access

Tailscale Serve, when installed, connected, and not disabled with
`IRIN_TAILSCALE_SERVE=0`, publishes the same War Room Web, Council API/WS,
and Gateway watch/health routes over the operator's private Tailscale
tailnet only. This is policy-bounded: every member or device permitted by
the operator's tailnet policy may reach the served UI, over
`https://<device>.<tailnet>.ts.net`. Tailscale Funnel (public internet
exposure) is never configured by IRIN. Shared-tailnet operators may retain
Council's token authentication as optional defense in depth; the default
single-operator path does not require manually copying a token to the phone.

## Source, configuration, and runtime boundaries

- **Source** is this Git repository: Rust and TypeScript code, cabinet
  YAML, Sentinel definitions, and documentation.
- **Configuration** is private and generated or operator-owned:
  `~/.config/irin/gateway.env` (Gateway and runtime settings, mode `0600`),
  `~/.irin/ledger_key.pem` (32-byte Ed25519 signing seed, mode `0600`), and
  provider API keys exported from the operator's login shell. None of these
  are read from or written into the repository.
- **Runtime state** is generated by running the product: Docker volumes
  (`gateway_sidecar_data`), `~/.local/state/irin/runtime/` (logs, source
  receipt, control lock), Council `sessions/`, `runs/`, and
  `librarian_chats/`. It is local-first data, not portable source, and is
  never committed.

## Related documents

- [`docs/cabinets.md`](cabinets.md) — cabinet selection, customization, and
  the optional NVIDIA starter.
- [`docs/troubleshooting.md`](troubleshooting.md) — setup, runtime, and
  recovery issues.
- [`docs/security-claims-vs-reality.md`](security-claims-vs-reality.md) —
  claim-by-claim security boundary.
- [`council-rs/docs/war-room.md`](../council-rs/docs/war-room.md) — War Room
  runtime shape and backend contract in detail.
- [`gateway/COUNCIL_GATEWAY_CONTRACT.md`](../gateway/COUNCIL_GATEWAY_CONTRACT.md)
  — Council/Gateway wire contract.
- [`sentinel/COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md) — Escalation
  and Directive message contract.
