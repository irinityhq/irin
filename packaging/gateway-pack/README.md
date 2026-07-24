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
| `~/Library/Application Support/com.irinity.irin/gateway/` | Pack data root | `0700` |
| `…/gateway/ledger_key` | Ledger signing seed (bind-mount only) | `0600` |
| `…/gateway/runtime.env` | Non-renderer secrets for Compose | `0600` |
| `private.json` | Non-secret: enabled flag, key id, pack version | `0600` |
| macOS Keychain (generic password) | Raw Council client `GW_API_KEY` | device-local access class |

Disable/stop keeps pack data. Destructive uninstall is a separate explicit
action and only targets the fixed `irin-desktop-gateway` project + app-owned
gateway directory (+ Keychain item for this app identity).

### `IRIN_APP_SUPPORT_ROOT` (test / portable-state)

Absolute path override for this app's Application Support directory only
(private.json, gateway pack tree, overlays, managed Docker CLI config). It does
**not** change Keychain selection, the login session, or the operator search
list. Packaged UI smoke uses this so app data stays isolated while the process
keeps the real `HOME` and existing login keychain. Never create a login
keychain under the override path.

## Image immutability

- Production manifests accept only `name@sha256:<64-hex>` image references.
- Tag-only references are refused.
- Before start, resolved image IDs/digests are verified against the manifest.
- App upgrades preserve pack data and Keychain items; a pack version / manifest
  change requires an explicit safe update/restart.

## Keychain continuity note

The installed app holds Gateway Pack secrets (`GW_API_KEY`, `AUTH_PEPPER`) as
device-local generic passwords under the service `com.irinity.irin`, matching
the stable Developer ID bundle identity. Items provisioned by the retired
"Council War Room" identity (`com.sovereign.council.warroom`) are copied
forward on first launch and never deleted by migration; if a copy is refused
by Keychain ACL, Enable simply re-provisions. Developer ID continuity across
app upgrades is proven before any release ships: the production dry-run gate
(`scripts/release-transaction.sh --dry-run-rc`) exercises first-run migration
and Keychain continuity under the real signed identity, and the PR may not
merge without it. Ad-hoc signed local builds may not retain Keychain access
across identity changes.

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

## Trust contract (v0.1)

### Trusted computing base

- The macOS user account, its login Keychain, and the Docker Desktop installation.
- IRIN.app and its bundled assets (code-signed; notarized in production).
- The Gateway Pack images pinned by digest in the installed manifest.
- The app-owned state under Application Support (`com.irinity.irin`), with
  pack asset integrity re-verified against the install marker before every
  secret-bearing Compose spawn (re-staged from the bundle on mismatch).

### How secrets move

- `GW_API_KEY` / `AUTH_PEPPER` live only in the login Keychain (device-local,
  `ThisDeviceOnly`) — never in files, env dumps, receipts, or logs.
- They leave the process only as (a) Compose spawn env scoped to lifecycle
  commands, force-scrubbed from every unrelated spawn, and (b) a bearer token
  to the app-owned Gateway, sent only after ownership is proven in three
  layers: project name, our installed compose file, and containers created
  from the validated manifest's pinned image digests.
- The app's Docker config is a managed minimal file (plugin hints only,
  0600). No operator registry credentials are read or copied.
- Watch producer/dispatcher/admin are force-disarmed on every spawn; ambient
  host tokens can never re-arm them.

### Out of scope (the documented boundary)

- A compromised host or same-user attacker: control of the Docker socket
  (which can read any container's env), substituted user binaries, or a
  poisoned GUI launch environment. `docs/security-claims-vs-reality.md` is
  canonical: local-first, single-operator, no sandbox against a compromised
  host. Review findings that require defeating this class are doctrine
  rejects, not defects.
- Non-macOS platforms (v0.1 is macOS arm64 only).

### Future hardening (recorded, not v0.1.0)

- Challenge-response ownership: the Gateway proves knowledge of `AUTH_PEPPER`
  before the app sends `GW_API_KEY` (sidecar endpoint).
- Registry-side tag immutability / environment protection on the GHCR packages.
- Lifecycle receipt SSOT for the packaged smokes, with accessibility-tree
  checks demoted to UX proof.
