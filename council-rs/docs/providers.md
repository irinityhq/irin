# Council Providers

Council can use authenticated local CLIs and provider APIs. Configuration is
operator-owned and remains outside the repository.

Run discovery before a live deliberation:

```bash
./target/release/council --base-dir council-rs --discover
```

A multi-seat council requires at least two usable transports. Discovery reports
what is configured or installed; it does not make a billable inference call and
does not claim that a credential is authenticated.

## Provider Matrix

| Seat ID | Exact transport | Credential or binary |
| --- | --- | --- |
| `grok_build` | Grok Build CLI | Authenticated `grok` CLI |
| `grok_hermes` | Grok through the Hermes adapter | Authenticated Hermes CLI/adapter |
| `grok_api` | xAI API | `XAI_API_KEY` |
| `claude_code` | Claude Code CLI | Authenticated `claude` CLI |
| `claude_api` | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `codex_cli` | Codex CLI | Authenticated `codex` CLI |
| `openai_api` | OpenAI Responses API | `OPENAI_API_KEY` |
| `gemini_agy` | Gemini through `agy` | Authenticated `agy` CLI |
| `gemini_vertex` | Gemini through Vertex AI | Google ADC and Vertex routing settings |
| `gemini_cli` | Legacy Gemini CLI | Authenticated `gemini` CLI |
| `nvidia`, `nim` | NVIDIA NIM API | `NVIDIA_API_KEY` |
| `nous` | Nous OpenAI-compatible API | `NOUS_API_KEY` |
| `deepseek` | Native or compatible API | `DEEPSEEK_API_KEY`, or a configured NIM/Nous model |

One seat ID always means one transport. Selecting a model must not silently
switch from a subscription CLI to a paid API or from Grok Build to Hermes.
Legacy IDs such as `grok`, `grok_cli`, `hermes_cli`, `claude`, `gpt`, `gemini`,
and `agy_cli` remain readable during migration, but new cabinets should use the
exact IDs above. `nim` remains a compatibility alias for `nvidia`.

## Utility Model Pins

The convergence judge prefers `grok-4.20-0309-reasoning`, then `grok-4.3`,
then its configured fallback. Frame check uses the more reliable short-prompt
order: `grok-4.3`, then `grok-4.20-0309-reasoning`, then its fallbacks.

Sheldon uses `grok-4.20-0309-reasoning` over Hermes as its primary evaluator.
Council gathers the evidence before that call: live X posts come from xmcp
`searchPostsRecent`; broader web evidence comes from Exa, Tavily and Tavily
News, Firecrawl for cited URLs, and optional Semantic Scholar. The Hermes
validator receives that bounded evidence in its prompt and is not described as
having a native search tool. If the gather returns no evidence, Council advances
to the `grok-cli-default` Grok Build fallback, which retains native web and X
search; the remaining cascade handles provider failures.

## How Discovery Builds the List

War Room requests `GET /api/discover` when the first component that needs
provider data mounts. The result is cached in the browser module until **Rescan**
is pressed. There is no background polling loop.

Each scan combines:

- a compiled catalog of known API and local transports, including unavailable
  rows so the operator can see what may be enabled;
- presence of non-empty API-key environment variables (names only are returned
  to the browser; values never are);
- supported local CLI and adapter binaries;
- local Ollama, LM Studio, and LocalAI endpoints;
- optional `~/.config/council/providers.toml` entries; and
- live model-catalog requests for supported configured APIs and local runtimes.

CLI model menus use curated routing lists because the supported CLIs do not all
offer the same stable machine-readable model-catalog command. Consequently the
transport list is discovered at runtime, while some model choices are curated
in source/routing YAML. Unavailable transports remain visible in Discover but
are disabled in cabinet, fork, and validator selectors.

Discovery also reports whether Gateway has an adapter for each exact transport.
An installed local transport can therefore be available for **Direct** mode but
marked **Direct only**. When **Governed via Gateway** is selected, War Room
blocks the start if the cabinet references one of those transports. Gateway's
current exact adapters are `grok_api`, `claude_api`, `claude_code`,
`openai_api`, `codex_cli`, `gemini_vertex`, `gemini_cli`, and `nvidia` (`nim`
is its legacy alias). Other discovered API, local-runtime, and custom transports
are marked Direct only unless a Gateway adapter is added. In particular,
Gateway does not silently substitute an API provider for `grok_build`,
`grok_hermes`, or `gemini_agy`.

User TOML entries may add new custom transport slugs, but cannot redefine a
reserved built-in ID. For example, a custom endpoint cannot call itself
`grok_build` or `claude_code`, because those names have fixed execution
semantics.

## Local CLI Seats

Local CLI seats reuse the operator's existing CLI authentication. Council does
not copy those credentials into the repository. Routing maps live in:

- `claude_routing.yaml`
- `grok_routing.yaml`
- `agy_routing.yaml`
- `gemini_routing.yaml`

The Hermes adapter can be disabled with `COUNCIL_HERMES_SEAT=0` or replaced by
setting `COUNCIL_HERMES_SEAT_BIN` to an operator-controlled adapter.

## API Seats

Export API credentials from your login shell. Add only the providers you use
to the shell profile, then start a new terminal. IRIN reads the inherited
environment and does not copy provider keys into its own configuration.

```bash
export XAI_API_KEY=<value>
export OPENAI_API_KEY=<value>
export ANTHROPIC_API_KEY=<value>
export NVIDIA_API_KEY=<value>
export NOUS_API_KEY=<value>
export DEEPSEEK_API_KEY=<value>

# Vertex seats use your existing Google ADC login plus these routing settings.
export VERTEX_PROJECT=<project-id>
export VERTEX_LOCATION=global
export VERTEX_GEMINI_MODEL=<model-id>
```

Restart the canonical runtime after changing provider configuration:

```bash
make runtime-restart
```

## Smoke Calls

The following commands make one live provider call and can incur cost:

```bash
./target/release/council --base-dir council-rs --smoke-provider claude_code "Reply with exactly: ACK"
./target/release/council --base-dir council-rs --smoke-provider grok_build "Reply with exactly: ACK"
./target/release/council --base-dir council-rs --smoke-provider nous "Reply with exactly: ACK"
./target/release/council --base-dir council-rs --smoke-provider nvidia "Reply with exactly: ACK"
```

Use `--smoke-model <model>` only when a specific configured model must be
checked. Use `--discover` when you only want to inspect availability without
making a billable provider call.

## Optional Routing Controls

| Variable | Effect |
| --- | --- |
| `COUNCIL_GROK_CLI_FALLBACK_API=1` | Migration-only fallback for legacy `grok`/`grok_cli` IDs; exact IDs never cross transports |
| `COUNCIL_HERMES_SEAT=0` | Disable Hermes routing |
| `COUNCIL_HERMES_SEAT_BIN` | Select a local Hermes adapter |
| `COUNCIL_CLAUDE_FORCE_API=1` | Legacy `claude` behavior only; use `claude_api` for an explicit API seat |
| `COUNCIL_INCLUDE_REASONING=1` | Include compatible reasoning blocks in seat text |
| `COUNCIL_NIM_ENABLE_THINKING=1` | Enable compatible NIM reasoning output |
| `COUNCIL_VIA_GATEWAY=1` | Route provider calls through Gateway |

Provider names and model identifiers change over time. The routing YAML and
`council --discover` output are authoritative for the installed commit.
