# Troubleshooting

IRIN is local-first software for one operator. Council and War Room Web
run on macOS and Ubuntu. The managed full-stack runtime (`make setup` and
`make runtime-*`), login recovery, Tailscale Serve automation, and native app
installation are currently macOS-only. On Ubuntu, `make warroom` runs the
browser deliberation surface; Gateway and the isolated verification lane use
their documented Docker paths. This page does not state measured build times or
minimum machine resources the repository does not itself prove; expect a
first macOS `make setup` to take noticeably longer than a subsequent one because
it compiles the Rust workspace, runs `npm ci`, and builds the Gateway/sidecar
Docker images from source.

## Prerequisites

On macOS, `make setup` checks for and tells you exactly what is missing: Docker
Desktop (running, not just installed), Rust (`cargo`, `rustc`), Node.js 20
or newer (`node`, `npm`), Git, `make`, `curl`, `jq`, OpenSSL, and the macOS
`lockf`/`launchctl` commands. Tailscale is optional and only checked if you
want private phone access. Fix the first reported missing command and rerun
`make setup` — it is safe to rerun. It preserves valid operator-owned values
and signing material while filling or migrating missing, placeholder, or
rejected IRIN-managed fields.

On Ubuntu, install Rust, Node.js 20 or newer, Git, `make`, `curl`, and `lsof`,
then use `make warroom`. Docker Engine plus the Compose and Buildx plugins are
required for Gateway and `make verify`, but not for the browser-only Council
launcher. `make setup`, `make runtime-*`, and `make app-install` deliberately
stop on Ubuntu because they depend on `lockf`, `launchctl`, Docker Desktop, and
the macOS application bundle. This is an installer boundary, not a claim that
Council or War Room Web cannot run on Linux.

## Docker

The macOS `make setup` path requires the Docker daemon to be running before it
starts — `docker info` must succeed or setup exits with an explicit instruction
to open Docker Desktop. Open it, wait until it reports ready, then rerun setup.
After installation, `make runtime-up` and the login-recovery controller can
open Docker Desktop and wait for the daemon (180 seconds by default,
configurable with `IRIN_DOCKER_WAIT_SECS`).

Building the Gateway and sidecar images the first time is a real Docker
build from this checkout, not a pull of a published image — expect it to use
meaningful local disk and CPU on a cold run.

If Docker reports an internal `no space left on device`, BuildKit metadata
input/output error, or cannot complete a build because its disk image is full,
restart the Docker daemon (Docker Desktop on macOS), then run
`make docker-cache-prune`. This removes only rebuildable BuildKit cache; it does
not remove images, containers, or named volumes. Do not substitute `docker
volume prune --all` or `docker compose down -v`: canonical named volumes contain
durable Gateway state. The next image build will be slower because its cache is
cold.

## Ports

The managed macOS runtime publishes its product services on these loopback
ports by default. Ubuntu `make warroom` starts only Council and War Room Web on
the first two ports; Gateway is a separate component-level start there.

| Service | Port |
| --- | --- |
| Council API/WebSocket | `8765` |
| War Room Web | `3010` |
| Gateway | `18080` |
| Claude CLI Gateway adapter (if `claude` CLI is present) | `9090` (host interfaces; token required) |
| Codex CLI Gateway adapter (if `codex` CLI is present) | `9091` (host interfaces; token required) |

If `make runtime-up` or `make setup` fails with a port already occupied
error, another process — often an old manual `council --serve`, `next
start`, or a previous IRIN runtime that did not shut down cleanly — owns
that port. Stop the desktop app or the old process, then retry. `make
verify` never conflicts with the canonical ports: it uses `28080` and
`28765` by default (`DEMO_GW_PORT`, `DEMO_COUNCIL_PORT`) in an isolated
Docker Compose project. A `make worktree` runtime gets its own
deterministic, non-conflicting port block derived from the worktree path.

The optional CLI adapters bind `0.0.0.0` so Gateway containers can reach them
through Docker Desktop's host bridge. Setup generates a distinct bearer token
for each adapter, and each proxy refuses a non-loopback bind without its token.
This means the listeners are reachable from host network interfaces but are not
unauthenticated. Keep the host behind a trusted private network/firewall and do
not forward ports `9090` or `9091`.

## Managed macOS runtime refuses the checkout

The managed runtime verifies source identity before it starts. A canonical
runtime must use the `irinityhq/irin` origin, the `main` branch, and a clean
tree; an isolated worktree must use the same origin and a non-`main` branch.
This is why `runtime origin is not irinityhq/irin`, `canonical runtime must
launch from main`, or `canonical runtime checkout is dirty` stops startup.

Commit the change in an IRIN-origin worktree, then update the clean canonical
checkout before restarting it. External fork contributors can build and run
the verification/test targets, but the managed product runtime intentionally
does not adopt a fork as its source. Do not change `origin` merely to bypass
this check.

## Login-shell provider discovery

Council reads provider API keys only from the environment your login shell
exports — it never reads or writes them into IRIN's own configuration. If
you add or change an `export XAI_API_KEY=...`-style line to your shell
profile, it will not take effect until:

1. you open a new terminal (or `source` the profile) so the shell actually
   exports it, and
2. you restart the canonical runtime with `make runtime-restart` so the
   already-running Council process picks it up.

Run `./target/release/council --base-dir council-rs --discover` after that
to confirm the provider now shows as available — it is a non-billable check.
If a key still does not show up, confirm it is exported in the same shell
IRIN's launchd services actually run under (`echo $SHELL`, and check that
the variable is not only set in an interactive-only block of your profile).

## Private phone access (macOS setup)

Tailscale is optional. If it is not installed or not connected, `make setup`
reports local access only and continues normally — this is not an error
state. If Tailscale is connected but no private phone URL is printed, rerun
`make setup`; the runtime configures Serve routes after the local stack is
healthy. Every member or device permitted by the operator's tailnet policy
may reach the served UI; Serve is private, not device-exclusive. To disable
Tailscale integration entirely, set
`IRIN_TAILSCALE_SERVE=0` before running setup or runtime commands. IRIN never
configures Tailscale Funnel or any other public-internet exposure — Serve
publishes only to devices on your own tailnet.

## Reboot and login recovery (macOS)

`make setup` installs a per-user macOS LaunchAgent
(`com.irinity.irin-runtime.login`) that runs at login and calls
`scripts/irin-runtime.sh boot`. On boot it reuses an already-healthy stack
whose build identity matches the checkout, or rebuilds and restarts if the
stack is down or the running build has drifted from the checkout. Its output
goes to `~/.local/state/irin/runtime/login-boot.log` — check that file first
if IRIN did not come back up after a reboot. To stop IRIN from starting at
login, run `./scripts/irin-runtime.sh uninstall-login`; manual `make
runtime-up` still works afterward.

`make runtime-status` reports both the checkout's Git identity and the
identity embedded in the currently running Council and Gateway sidecar
builds; a `RUNTIME_MISMATCH` line means the running services do not match
the checkout on disk (dirty tree, unbuilt commit, or a source-receipt/
build drift). Commit worktree changes and update the clean canonical checkout;
then `make runtime-restart` rebuilds from that committed source.

## Watch looks empty or quiet

An empty Outbox or a quiet Watch tab is expected behavior, not a health
problem: the canonical local runtime loads exactly one deterministic
test Sentinel, and the watch dispatcher and producer are disabled by
default. Watch's War Room view is also a bounded, sanitized snapshot (recent
fire counts and a capped recent-fires list), not the full underlying ledger
— see [`docs/architecture.md`](architecture.md) for how it relates to the
signed Outbox. If Council and Gateway both report healthy in `make
runtime-status`, a quiet Watch tab is not itself a fault.

## Tauri desktop app (macOS)

The installed release app never starts its own Council backend — it probes
the configured Council URL and adopts the canonical runtime if the build
identity matches. If the app reports it cannot reach Council, the fix is to
make sure the canonical runtime is up (`make runtime-status`, `make
runtime-up`), not to restart the app repeatedly. `make app-install` rebuilds,
atomically replaces `/Applications/Council War Room.app`, and relaunches it;
if automatic launch fails after install, the script tells you the exact
`open '...'` command to run by hand. See
[`council-rs/warroom/docs/TAURI-AUTH.md`](../council-rs/warroom/docs/TAURI-AUTH.md)
for auth-token behavior across release and debug builds.

## Teardown

```bash
make runtime-down     # stop the canonical local product runtime
make verify-down      # tear down only the isolated verification stack
```

Do not run `docker compose down -v` against the canonical `gateway` Compose
project by hand — the `-v` flag deletes durable Gateway state (the watch
plane, ledger, and outbox). `make verify-down` is safe to run at any time
because the verification stack is fully isolated (its own Compose project,
ports, and ephemeral signing key) and never touches canonical state.

## Where to look next

- Local runtime logs: `~/.local/state/irin/runtime/` — `council.log`,
  `web.log`, `supervisor.log`, `claude-proxy.log`, `codex-proxy.log`,
  `login-boot.log`.
- Gateway/sidecar logs: from the repository root, use
  `docker compose --env-file /dev/null -p gateway -f gateway/docker-compose.yml -f gateway/docker-compose.canary.yml logs --tail=100 sidecar`
  (replace `sidecar` with `gateway` for the proxy container).
- Health endpoints: `curl -fsS http://127.0.0.1:8765/api/health`,
  `curl -fsS http://127.0.0.1:18080/health`.
- Gateway-specific failure guide:
  [`gateway/docs/runbook.md`](../gateway/docs/runbook.md) (failure table,
  backup/recovery, signing-key issues).
- Provider/discovery detail:
  [`council-rs/docs/providers.md`](../council-rs/docs/providers.md).
- Security boundary and what is and is not enforced:
  [`docs/security-claims-vs-reality.md`](security-claims-vs-reality.md).
