# Tauri War Room — Authentication

How the desktop shell and browser reference UI authenticate against
`council --serve`, including Council's Gateway governance proxy.

## Council API / WebSocket (`:8765`)

### Release (Tauri production build)

1. Start the canonical runtime with root `make setup`. The installed app adopts
   that Council and never starts its own backend.
2. The default private template leaves `COUNCIL_AUTH_TOKEN` empty for the
   loopback-only single-operator runtime. If the operator explicitly configured
   a bearer token, set the same operator-managed value in **Settings → Auth
   token**. Setup does not copy or print it.
3. REST calls send `Authorization: Bearer <token>`.
4. WebSocket upgrade offers `Sec-WebSocket-Protocol` values `council` and
   `token.<token>` (browsers cannot set custom WS headers). The server validates
   `token.<token>` with constant-time compare, then **negotiates `council`** in the
   101 response so `WebSocket.protocol` is `council` in the UI.

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

## Runtime overrides (localStorage)

`warroom/web/lib/runtime-config.ts` load order:

1. `localStorage` key `warroom.runtime-config.v1`
2. `NEXT_PUBLIC_*` build-time defaults

`configReady` resolves after the first `loadRuntimeConfig()` so health checks and
WebSocket connects use hydrated URLs/tokens.

Changing Settings does not require re-running `npm run build:tauri`.

Prefer loopback URLs (`127.0.0.1` / `localhost`) — Settings warns on non-loopback
hosts because the auth token would be sent to remote machines if misconfigured.

## Manual release checklist

1. From the IRIN root, run `make setup`, then `make app-install`.
2. Launch app → **Settings** → set an auth token only if the canonical runtime
   uses one → **Test connection** (Council API green).
3. Confirm the app reports adoption of the canonical Council and does not own a
   child backend.
4. Open Watch and Outbox; both load through the Council API without a browser
   Gateway credential.
5. Tray **Convene** focuses Deliberate view; if Council is unavailable, recover
   it from the IRIN checkout rather than starting another backend.
6. Run **Checklist Duo** (1 round) → synthesis → **Export PDF** → native OS save
   dialog; file lands where chosen.

## Related

- `docs/war-room.md` — operator map
