# Council Operator Guide

Council is IRIN's multi-model deliberation engine. War Room is its local
desktop and browser interface. Council and the browser War Room run on macOS
and Ubuntu; the managed full-stack runtime and desktop installation are
currently macOS-only.

## Start the product

On macOS, from the IRIN repository root:

```bash
make setup
```

Setup reports the live service state and URLs, enables per-user login recovery,
and ends with the next action: open **Discover**. The only optional second
newcomer command is `make app-install`. Operators can opt out of login recovery
with `./scripts/irin-runtime.sh uninstall-login`.

Default local surfaces:

| Surface | Address |
|---|---|
| Council API and WebSocket | `http://127.0.0.1:8765` |
| War Room Web | `http://127.0.0.1:3010` |
| Gateway | `http://127.0.0.1:18080` |

The services bind to loopback. The root runtime controller can optionally
publish selected routes through the operator's private Tailscale network.

On Ubuntu, start Council and the browser War Room in the foreground:

```bash
make warroom
```

Open `http://127.0.0.1:3010` and stop both processes with `Ctrl+C`. This path
does not start Gateway or install login recovery; use the Gateway component
documentation when that governed path is needed.

## Configure providers

Provider API keys come from the login-shell environment; IRIN does not copy
them. Authenticated local CLIs keep using their own credential stores. See
[providers.md](providers.md) for supported transports and variable names.

Discovery is non-billable:

```bash
./target/release/council --base-dir council-rs --discover
```

A live smoke call can incur cost:

```bash
./target/release/council --base-dir council-rs \
  --smoke-provider claude_code "Reply with exactly: ACK"
```

## Use the CLI

```bash
# Standard cabinet, default tear-down mode
./target/release/council --base-dir council-rs "Should we ship Friday?"

# Constructive pathfinding
./target/release/council --base-dir council-rs --pathfind \
  "Find a safe migration path"

# Focused code review
./target/release/council --base-dir council-rs --harden --map ./src \
  "Review this module"
```

Cabinet selects the seats and round count. Mode selects how the seats reason:

| Mode | Flag | Behavior |
|---|---|---|
| Tear-down | default | Stress the proposal and permit a no-go result. |
| Pathfind | `--pathfind` | Pair objections with a path or scope reduction. |
| Harden | `--harden` | Pair adversarial findings with concrete fixes. |
| Pathfind then tear-down | `--pathfind --then-tear-down` | Generate options, then challenge the winner. |

Useful commands:

```bash
./target/release/council --base-dir council-rs --quick "Topic"
./target/release/council --base-dir council-rs --cabinet warroom "Topic"
./target/release/council --base-dir council-rs --recall "search terms"
./target/release/council --base-dir council-rs --budget 0.50 "Topic"
./target/release/council --base-dir council-rs --context notes.md "Topic"
```

## Use War Room

The browser and Tauri app use the same Council API and WebSocket contract.

Open the browser surface at `http://127.0.0.1:3010`. On macOS, the native app is
also available:

```bash
make app-install
```

War Room includes deliberation, direct-fire prompts, session history, provider
discovery, cabinet editing, Gateway outbox and Watch views, drift analysis,
and optional Librarian integration. Configure API, WebSocket, Gateway,
Librarian, and auth values in Settings.

The Tauri app adopts the canonical Council started by `make setup`. Installed
release builds do not start another Council backend; if the runtime is absent,
restart it from the IRIN checkout. Debug desktop builds retain a developer-only
sidecar path.

## Authentication

The canonical runtime loads `COUNCIL_AUTH_TOKEN` and
`COUNCIL_GATEWAY_TOKEN` from private local configuration. War Room stores its
runtime endpoints and token in local app/browser state.

Development can use `COUNCIL_DEV_NO_AUTH=1` on loopback. Do not use that flag
for a network-accessible service. See
[`warroom/docs/TAURI-AUTH.md`](../warroom/docs/TAURI-AUTH.md).

## Persistence

Sessions, indexes, run summaries, and Librarian chat wrappers are runtime data
and stay outside Git. Their paths and migration checks are documented in
[persistence.md](persistence.md).

## Verify

No-provider checks:

```bash
cargo test -p council-rs --all-targets --all-features
cd council-rs/warroom/web
npm run lint
npm run typecheck
npm test
```

The root `make verify` target proves the isolated Sentinel-to-signed-directive
path without provider credentials or hardware arming.

Use `make runtime-status` for liveness. Neither command proves that a paid
provider call or an armed action path has occurred.
