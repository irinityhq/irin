# Troubleshooting

IRIN is local-first software for one operator. Council and War Room Web
run on macOS and Ubuntu. Foreground `make warroom`, optional Gateway compose,
Tailscale Serve (installed app only), and native app
installation are currently macOS-only. On Ubuntu, `make warroom` runs the
browser deliberation surface; Gateway and the isolated verification lane use
their documented Docker paths. This page does not state measured build times or
minimum machine resources the repository does not itself prove; expect a
first Gateway image build to take noticeably longer than a subsequent one because
it compiles the Rust workspace, runs `npm ci`, and builds the Gateway/sidecar
Docker images from source.

## Prerequisites

On macOS, `make gateway-prepare-config` requires OpenSSL and prepares only the
private Gateway environment and ledger key; it does not check Docker, Rust,
Node.js, Git, `jq`, `lockf`, or `launchctl`, and it starts no services. It is
safe to rerun: valid operator-owned values and signing material are preserved
while missing or placeholder IRIN-managed fields are filled. Gateway Pack
enable separately requires Docker Desktop to be running.

On Ubuntu, install Rust, Node.js 20 or newer, Git, `make`, `curl`, and `lsof`,
then use `make warroom`. Docker Engine plus the Compose and Buildx plugins are
required for Gateway and `make verify`, but not for the browser-only Council
launcher. Installed-app and Gateway Pack paths are macOS-first because
they depend on `lockf`, `launchctl`, Docker Desktop, and macOS-only runtime
recovery. This is an installer boundary, not a claim that Council or War Room
Web cannot run on Linux.

## Docker

Gateway Pack enable requires the Docker daemon to be running before it
starts — `docker info` must succeed or setup exits with an explicit instruction
to open Docker Desktop. Open it, wait until it reports ready, then rerun setup.
After installation, the optional CLI proxy launcher can
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

If `make warroom` or the installed app fails with a port already occupied
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
2. you relaunch `make warroom` or IRIN.app so the
   already-running Council process picks it up.

Run `./target/release/council --base-dir council-rs --discover` after that
to confirm the provider now shows as available — it is a non-billable check.
If a key still does not show up, confirm it is exported in the same shell
that launches `make warroom` or IRIN.app (`echo $SHELL`, and check that
the variable is not only set in an interactive-only block of your profile).

## Private phone access (installed IRIN.app)

Private phone access is controlled in the installed IRIN.app under Settings
→ Private phone access. Source development only
start local loopback services and do not configure Tailscale Serve. Enable
phone access after Council is ready: IRIN publishes on dedicated HTTPS port
`8443` by default (`IRIN_TAILSCALE_HTTPS_PORT` overrides it) and does not claim
port 443, so another Serve root on 443 can remain. The ready URL is
`https://<MagicDNS>:8443` (include the port). Open that origin in a browser on
any device that is both on the same tailnet and allowed by the operator's
Tailscale ACLs or grants; War Room uses same-origin REST and WebSocket. If
Council requires an auth token, set it under Settings → Auth token on that
browser and use Test connection. The token remains only for that browser tab's
session and is not written to durable localStorage. Serve is private, not
device-exclusive.
Disable from the same Settings control — product code uses port-scoped `off`
and never global `tailscale serve reset`. IRIN never configures Tailscale
Funnel or any other public-internet exposure.

## Reboot and login recovery (macOS)

Login recovery for a source-managed runtime is retired. The installed app
owns its bundled Council; source development uses foreground `make warroom`.

Packaged and source Council health endpoints report build identity; the
identity embedded in the currently running Council and Gateway sidecar
builds; a `RUNTIME_MISMATCH` line means the running services do not match
the checkout on disk (dirty tree, unbuilt commit, or a source-receipt/
build drift). Commit worktree changes and update the clean canonical checkout;
then rebuild and relaunch from that committed source.

## Watch looks empty or quiet

An empty Outbox or a quiet Watch tab is expected behavior, not a health
problem: the canonical local runtime loads exactly one deterministic
test Sentinel, and the watch dispatcher and producer are disabled by
default. Watch's War Room view is also a bounded, sanitized snapshot (recent
fire counts and a capped recent-fires list), not the full underlying ledger
— see [`docs/architecture.md`](architecture.md) for how it relates to the
signed Outbox. If Council and Gateway both report healthy, a quiet Watch tab
is not itself a fault.

## Desktop app (macOS)

The installed release app (signed DMG) always owns and starts its bundled
Council backend. An occupied Council port is a startup conflict — the app does
not adopt another process. An installed DMG does not require a source checkout
or `make warroom`. Product installation is the DMG only — there is no
supported source-built app installer. See
[`council-rs/warroom/docs/TAURI-AUTH.md`](../council-rs/warroom/docs/TAURI-AUTH.md)
for auth-token behavior across release and debug builds.

The native visual proof in `make ship-check`
(`scripts/smoke-macos-tauri-app.sh`) requires a real on-screen window in an
interactive GUI session. It fails with `no on-screen window for pid` when the
display is asleep, the session is locked without a visible desktop, or the
machine is genuinely headless — even though the app process is healthy.
`caffeinate -dims` alone only *prevents* later sleep; it does not wake an
already-off display. Wake the display first (`caffeinate -u -t 1` turns it on
briefly), then keep it awake for the run:

```bash
caffeinate -u -t 1
caffeinate -dims make ship-check
```

A single `caffeinate -dimsu make ship-check` also works on many machines
(`-u` asserts user activity and can turn the display on). Headless or
SSH-only hosts without a GUI session still cannot satisfy this proof.

## Teardown

```bash
Ctrl+C on make warroom  # stop foreground source development
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
