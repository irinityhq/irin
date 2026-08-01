# War Room

War Room is the local operator UI for Council. A Next.js application under
`warroom/web/` serves both the browser surface and the Tauri desktop shell
under `warroom-tauri/`.

Operator setup: [operator-guide.md](operator-guide.md).
Provider routing: [providers.md](providers.md).

## Runtime shape

```text
Browser or Tauri
    | REST + WebSocket
    v
Council server :8765
    | optional governed calls
    v
Gateway :18080
```

Foreground `make warroom` serves War Room Web on `127.0.0.1:3010` and Council
on `127.0.0.1:8765`. The installed DMG owns bundled Council and serves the
packaged War Room export from that process. Optional Gateway is
`127.0.0.1:18080` when the app Gateway Pack is enabled.

The Tauri shell always owns its Council child. If the Council port is already
occupied, the app reports a startup conflict and does not adopt the other
process. A source checkout is not a prerequisite for the installed app.
Debug desktop builds may start a repo-built `council` binary for development.

## Run and build

Browser development from the repository root:

```bash
make warroom
```

Internal native shell harness (component developers and CI). Tauri is the
implementation layer; the sole production IRIN.app factory is
`make dmg-build` / the release transaction:

```bash
make -C council-rs warroom-dev
```

For browser-only development without the root launcher:

```bash
cargo build --release
./target/release/council --base-dir . --serve --port 8765
cd warroom/web
npm ci
npm run dev:local
```

## Operator surfaces

War Room exposes these primary workflows:

- **Deliberate** streams a multi-seat Council session and permits operator
  intervention between rounds.
- **Direct Fire** sends a focused single-seat prompt.
- **History** reads saved sessions, synthesis, lineage, and exports.
- **Outbox** reads signed Gateway directives.
- **Watch** reads registered Sentinels, fires, and watch-plane status.
- **Discover** scans exact provider transports. It shows unavailable paths for
  setup guidance while cabinet, fork, and validator selectors disable them.
- **Cabinets** reads and edits local cabinet YAML.
- **Drift** compares normal and blind reruns.
- **Librarian** proxies an optional separately configured local service.
- **Settings** owns runtime endpoints, auth, Council root, and app controls.

An empty Outbox does not mean Council is unhealthy. It means no signed
directive is available from the configured Gateway. A Watch view can be
readable while action production remains disabled.

## Backend contract

`src/server.rs` owns the REST and WebSocket server. Core endpoints include:

- `GET /api/health`
- `GET /api/discover`
- `GET /api/cabinets`
- `POST /api/cabinets/save`
- `POST /api/deliberate`
- `GET /api/sessions`
- `GET /api/precedent`
- `/api/drift/*`
- `/api/mapmaker/*`
- `/api/meta-review/*`
- `/api/librarian/*`
- `GET /ws/deliberate`

The Gateway service endpoint can require `X-Gateway-Auth`. Interactive REST
uses bearer auth when `COUNCIL_AUTH_TOKEN` is set. The WebSocket client carries
the same token through its negotiated subprotocol.

## Deliberation WebSocket

The client starts a session with a `start` message and can send an
`intervention` while the server is awaiting input. Server events cover session,
round, seat, synthesis, persistence, and error state. Important streamed events
include:

- `session_started`
- `round_started`
- `seat_started`
- `seat_chunk`
- `seat_complete`
- `convergence_scored`
- `round_divergence`
- `round_complete`
- `awaiting_input`
- `intervention_received`
- `synthesis_started`
- `synthesis_complete`
- `session_saved`
- `done`
- `error`

The TypeScript definitions in `warroom/web/lib/` and Rust server enums are the
wire-shape authority. Change them together.

## Runtime settings

`warroom/web/lib/runtime-config.ts` stores the API base, WebSocket base,
Gateway base, Librarian base, auth token, and Council root in local storage.
`NEXT_PUBLIC_*` values provide build-time defaults.

Prefer loopback URLs. Non-loopback endpoints can transmit the configured auth
token to another host and should be used only across a trusted private
transport.

## Librarian integration

The Librarian tab is optional. Council owns the UI and local chat wrapper;
the configured Librarian service owns retrieval and generation. Set its base
URL in Settings. When unavailable, the tab reports an offline state without
blocking Council or Gateway workflows.

## Tests

From the IRIN repository root:

```bash
make -C council-rs warroom-check
```

The gate runs frontend lint, typecheck, static export, unit tests, and the
hybrid Tauri build smoke. Browser tests exercise the health request, page
render, WebSocket handshake, and session-start event without requiring a paid
provider.

For focused frontend work:

```bash
make -C council-rs warroom-web-check

cd warroom/web
npm run lint
npm run typecheck
npm test
```

Session persistence and local runtime data are documented in
[persistence.md](persistence.md).
