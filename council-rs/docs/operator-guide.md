# Council Operator Guide

Council is IRIN's multi-model deliberation engine. War Room is its local
desktop and browser interface. Council and the browser War Room run on macOS
and Ubuntu; the desktop installation is currently macOS-only.

## Start the product

Product install: download the signed DMG and launch `IRIN.app`. The app owns
its bundled Council.

Source browser development, from the IRIN repository root (macOS or Ubuntu):

```bash
make warroom
```

Open `http://127.0.0.1:3010` and stop with `Ctrl+C`. Source development does
not install login recovery.

Default local surfaces:

| Surface | Address |
|---|---|
| Council API and WebSocket | `http://127.0.0.1:8765` |
| War Room Web (source) | `http://127.0.0.1:3010` |
| Gateway | optional app Gateway Pack, or compose from `gateway/` |

The services bind to loopback. Private phone publication is controlled only
from installed IRIN.app Settings via Tailscale Serve.

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

Open the browser surface at `http://127.0.0.1:3010`. On macOS, install the
signed DMG for the native desktop product (`IRIN.app`).

War Room includes deliberation, direct-fire prompts, session history, provider
discovery, cabinet editing, Gateway outbox and Watch views, intervention
patterns, drift analysis, meta-review, and optional Librarian integration.
Configure API, WebSocket, Gateway, and auth values in Settings. The installed
app's Settings also owns the Gateway Pack lifecycle (enable, disable, stop,
uninstall), Touch ID arming of the watch producer, and Tailscale phone
access. The watch-sentinels profile toggle and inbox opener live on the
Watch view, not in Settings. The Gateway Pack is optional and needs
Docker; core War Room works in Direct mode without Docker.

The installed app always owns its bundled Council. Foreground `make warroom`
is a separate development process tree. An occupied Council port is a startup
conflict, not a second ownership mode. Debug desktop builds retain a
developer-only sidecar path under `council-rs`.

## Authentication

Installed release builds manage pairing auth in private Application Support.
War Room stores non-secret endpoints and a session-only auth token in browser
state.

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

Use Council `/api/health` for liveness. Neither command proves that a paid
provider call or an armed action path has occurred.
