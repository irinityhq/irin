# IRIN (Desktop)

Primary operator UI for council-rs: a **Tauri v2** desktop shell hosting the full
**Next.js** War Room from `warroom/web/` (including Deliberation, Outbox,
Librarian, and Drift). Installed release builds adopt the canonical Council on
**8765** started by root `make setup`; they never start a second backend. Debug
desktop builds retain a developer-only `council --serve` sidecar.

## Product install versus component development

Product installation is owned by the root [IRIN README](../../README.md): run
root `make setup`, with root `make app-install` as the optional desktop-app
step. The commands below are component developer and packaging commands, not an
alternate installation path.

Development-only overrides:

- `COUNCIL_RS_DIR` — repo root (default: parent of `warroom-tauri/`)
- Council binary path — Settings UI or explicit path to `target/release/council`

**Ports:** API/WS default **8765**. Watch and Outbox use Council's authenticated
`/api/governance/*` proxy; Gateway's default **18080** base is only an optional
direct health probe in Settings. The desktop connection still accepts only port
**8765**; debug sidecar spawning uses that same port.

**Settings:** Gear icon in the War Room nav (not Cabinets). Persist API/WS
bases, an optional Gateway health base, auth token, and optional council binary path. See
`warroom/docs/TAURI-AUTH.md`.

## Tauri development

From the IRIN repository root:

```bash
make -C council-rs warroom-dev
```

This starts Next dev on **3010** (`dev:local`) inside the webview. The debug
Tauri host auto-starts `council --serve` when `target/release/council` exists
(see Backend logs in the UI if the binary is missing).

Production bundle (static export + Tauri):

```bash
make -C council-rs warroom-build
# artifact under warroom-tauri/src-tauri/target/release/bundle/
```

Assets only (no full `tauri build`):

```bash
make -C council-rs warroom-export
# writes warroom-tauri/warroom-web-dist/ from warroom/web/.next-tauri/
```

## Browser War Room (reference)

```bash
cargo build --release -p council-rs --bin council
./target/release/council --base-dir council-rs --serve --port 8765
cd council-rs/warroom/web && npm run dev
```

See `../warroom/README.md` and `../docs/war-room.md`.

## Tests

```bash
# Full gate from repo root
make -C council-rs warroom-check

# Or step-by-step
bash council-rs/warroom-tauri/scripts/smoke-hybrid-build.sh

# Or from warroom/web (lint/typecheck/export gate)
cd council-rs/warroom/web && npm run lint && npm run typecheck && npm run build:tauri
test -f .next-tauri/index.html
```

### Manual Tauri smoke (local)

1. Debug: from the IRIN root, run `cargo build --release -p council-rs --bin
   council`, then `make -C council-rs warroom-dev`.
2. Confirm the debug sidecar serves `/api/health`, cabinets, and the
   Outbox/Librarian tabs.
3. Release: follow the root README's product installation path.
4. Confirm the installed app adopts the already-running Council and Discover
   matches the browser War Room.

Use `COUNCIL_WS_SMOKE_ONLY=1` on the backend for WebSocket proof without provider spend:

```bash
COUNCIL_WS_SMOKE_ONLY=1 COUNCIL_DEV_NO_AUTH=1 ./target/release/council --serve --port 8765
```

**Auth:** Debug desktop builds set `COUNCIL_DEV_NO_AUTH=1` only on their debug
sidecar. Release bundles do not own Council auth. If the canonical runtime has
an operator-configured `COUNCIL_AUTH_TOKEN`, set the same operator-managed value
in War Room Settings; the app sends it only to the loopback Council and does not
print it.
