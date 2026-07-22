# IRIN Desktop Gateway Pack (v0.1)

Optional, app-owned Gateway runtime for the installed Apple-silicon DMG.

## Product contract

- **Core DMG is Docker-free.** Gateway is off by default. Missing Docker is
  non-red for core War Room (Direct mode).
- **Optional is real.** With Docker Desktop running, the installed app can
  install, start, provision, enable, disable, and stop an app-owned Gateway.
- **Authenticated readiness is required** before governed proceedings. A mere
  Gateway URL is not "ready."
- **Never touches the canonical checkout project** (`gateway/` Compose project
  or its volumes). Fixed desktop project: `irin-desktop-gateway`.

## Support matrix (v0.1)

| Path | Pack (Governed) | Direct (no pack) | Notes |
| --- | --- | --- | --- |
| xAI / OpenAI / Anthropic / NVIDIA API keys | Supported when login-shell provider env is present | Supported | Keys injected only by native code into app-owned 0600 runtime files; never from the renderer |
| Vertex / gcloud ADC | **Not supported** | Supported when host ADC is available | No host `~/.config/gcloud` mount; keep Vertex Direct-only |
| Claude CLI / Codex CLI proxies | **Not supported** | Supported when CLIs are installed/authenticated | DMG does not install or authenticate those CLIs |
| Watch producer / dispatcher arming | **Disabled / not exposed** | N/A | Forced `false` in every pack path; no arming control in the UI |

## Runtime assets (bundled)

Staged into the app bundle under `Contents/Resources/gateway-pack/` at DMG
build time (gitignored staging):

- `docker-compose.yml` — no `build:` directives, no `${HOME}` mounts
- `nginx.conf`, `conf/`, `lua/` — runtime-only copies from `gateway/`
- `image-manifest.json` — **production** must use exact `name@sha256:digest`
  refs for gateway, sidecar, and third-party base images

Local non-publishing regression uses a separate **development builder** that
writes a test-only local manifest under `packaging/build/gateway-pack/`. That
path does not weaken the production digest requirement.

## App-owned state

| Path | Purpose | Permissions |
| --- | --- | --- |
| `~/Library/Application Support/com.sovereign.council.warroom/gateway/` | Pack data root | `0700` |
| `…/gateway/ledger_key` | Ledger signing seed (bind-mount only) | `0600` |
| `…/gateway/runtime.env` | Non-renderer secrets for Compose | `0600` |
| `private.json` | Non-secret: enabled flag, key id, pack version | `0600` |
| macOS Keychain (generic password) | Raw Council client `GW_API_KEY` | device-local access class |

Disable/stop keeps pack data. Destructive uninstall is a separate explicit
action and only targets the fixed `irin-desktop-gateway` project + app-owned
gateway directory (+ Keychain item for this app identity).

## Image immutability

- Production manifests accept only `name@sha256:<64-hex>` image references.
- Tag-only references are refused.
- Before start, resolved image IDs/digests are verified against the manifest.
- App upgrades preserve pack data and Keychain items; a pack version / manifest
  change requires an explicit safe update/restart.

## Keychain continuity note

Final stable Developer ID signing must prove Keychain item continuity across
app upgrades under the stable app identity
`com.sovereign.council.warroom`. Ad-hoc signed local builds may not retain
Keychain access across identity changes; that is a release ceremony item, not
proven by local DMG smoke.

## Operator flow (installed release)

1. Install Docker Desktop; wait until the daemon is ready.
2. Settings → **Enable Gateway** (installs pack resources into app support if
   needed, starts `irin-desktop-gateway`, provisions a service-role Council
   client key into Keychain, proves `GET /v1/models`).
3. On authenticated ready, bundled Council restarts with Keychain-sourced
   `GW_API_KEY` + fixed loopback `GATEWAY_URL` and `COUNCIL_VIA_GATEWAY=1`.
4. **Disable** reverses to Direct and removes the key from the child env.
5. **Stop pack** stops containers only. **Uninstall pack** is destructive and
   explicit.
