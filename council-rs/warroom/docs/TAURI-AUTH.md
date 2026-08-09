# Tauri War Room — Authentication

How the desktop shell and browser reference UI authenticate against
`council --serve`, including Council's Gateway governance proxy.

## Council API / WebSocket (`:8765`)

### Release (Tauri production build)

1. Installed IRIN.app (the DMG product) starts and owns its bundled Council; a
   source checkout is not required. If the configured Council port is already
   occupied, the app reports a conflict and does not adopt the listener.
2. The default private template leaves `COUNCIL_AUTH_TOKEN` empty for the
   loopback-only single-operator runtime. If the operator explicitly configured
   a bearer token, set the same operator-managed value in **Settings → Auth
   token**. Setup does not copy or print it.
3. REST calls send `Authorization: Bearer <token>`.
4. WebSocket upgrade offers `Sec-WebSocket-Protocol` values `council` and
   `token.<token>` (browsers cannot set custom WS headers). The server checks
   `Origin` first, then validates `token.<token>` with constant-time compare,
   then **negotiates `council`** in the 101 response so `WebSocket.protocol` is
   `council` in the UI.
5. The `Origin` gate applies to every WebSocket upgrade, including the
   token-less loopback posture. A present `Origin` must satisfy the same allow
   predicate as HTTP CORS — loopback plus configured origins such as
   `tauri://localhost` — or the upgrade is refused with 403; a correct token
   does not override it. A request with no `Origin` header is a non-browser
   local client and stays allowed.

`COUNCIL_DEV_NO_AUTH` is not set by the installed release app.

### Debug (Tauri `cargo tauri dev`)

Auto-start and **Start server** set `COUNCIL_DEV_NO_AUTH=1` on the sidecar.
You may leave Settings token empty for loopback dev. To test release-like auth,
set `COUNCIL_AUTH_TOKEN` on a manually started `council --serve` and the same
token in Settings.

### Browser reference (`npm run dev:local`)

Either:

- `COUNCIL_DEV_NO_AUTH=1` on `council --serve` (loopback only), or
- `COUNCIL_AUTH_TOKEN` on the server and the same value in Settings /
  `NEXT_PUBLIC_COUNCIL_AUTH_TOKEN` in `.env.local`.

## Gateway Watch and Outbox

The browser fetches Watch and Outbox data from Council's authenticated,
GET-only `/api/governance/*` proxy. Council reads `WATCH_ADMIN_TOKEN` (falling
back to `BOOTSTRAP_TOKEN` on older installs) from its private process
environment and sends it to Gateway; the credential never enters browser
configuration or response data.

**Gateway health base** in Settings is optional and is used only by **Test
connection** for a direct health probe. It does not configure Watch or Outbox.

## Runtime overrides (durable endpoints, session-only auth)

`warroom/web/lib/runtime-config.ts` keeps non-secret endpoint overrides in the
`localStorage` key `warroom.runtime-config.v1`. The auth token uses the
`sessionStorage` key `warroom.runtime-auth.v1`, so it survives a reload in the
current tab but is discarded when that tab's session ends. Loading a value
written by an older build scrubs `authToken` from durable localStorage instead
of hydrating it.

`configReady` resolves after the first `loadRuntimeConfig()` so health checks and
WebSocket connects use hydrated URLs and the current session token.

Changing Settings does not require re-running the Tauri asset export
(`npm run build:tauri` → `warroom-tauri/scripts/build-warroom-assets.sh`).

Prefer loopback URLs (`127.0.0.1` / `localhost`) — Settings warns on non-loopback
hosts because the auth token would be sent to remote machines if misconfigured.

## Remote browser (private Tailscale Serve)

When phone access is enabled in the installed app, open the served HTTPS
origin (default port `8443`) in a browser that the operator's Tailscale ACLs or
grants allow. Remote pages default to same-origin API, WebSocket, and Gateway
bases. Set **Settings → Auth token** when Council requires one, then **Test
connection** (REST health and WebSocket upgrade). The token remains in the
current browser tab's session only — not durable localStorage, a native
Keychain, or a separate phone app.

## Manual release checklist

1. Install the signed DMG (or use an already-accepted local build), or run
   `make warroom` for source browser development.
2. Launch app → **Settings** → set an auth token only if the runtime uses one
   → **Test connection** (Council API green).
3. Confirm the app reports a healthy app-owned Council backend.
4. Open Watch and Outbox; both load through the Council API without a browser
   Gateway credential.
5. Tray **Convene** focuses Deliberate view; if Council is unavailable, recover
   it from the installed app lifecycle or the IRIN checkout.
6. Run **Checklist Duo** (1 round) → synthesis → **Export PDF** → native OS save
   dialog; file lands where chosen.

## Related

- `docs/war-room.md` — operator map
