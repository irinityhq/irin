# Council

Council is IRIN's multi-model deliberation engine. It provides a CLI, an HTTP
and WebSocket server, the War Room Web interface, and a Tauri desktop shell.

## Build

From the repository root:

```bash
cargo build --release -p council-rs --bin council
```

Run provider discovery:

```bash
./target/release/council --base-dir council-rs --discover
```

Run a deliberation:

```bash
./target/release/council --base-dir council-rs --quick "Review this decision"
```

Provider calls may incur cost. Use a deterministic smoke or the root
`make verify` lane when live calls are not intended.

## War Room

On macOS, the canonical local runtime starts Council on `127.0.0.1:8765` and
War Room Web on `127.0.0.1:3010`:

```bash
make warroom
```

On macOS or Ubuntu, the foreground browser-only path is:

```bash
make warroom
```

Development and packaging targets are available under `make -C council-rs
help`. The Tauri app always attempts to start and own its bundled Council. An
occupied Council port is reported as a conflict; the app never adopts that
listener.

## Implementation map (for feature work)

- War Room Web (receipts / UI snapshot client): `warroom/web/`
- Change-class checklists (e.g. redacted execute receipt):
  [`../docs/seams/`](../docs/seams/)

## Documentation

- [`docs/operator-guide.md`](docs/operator-guide.md)
- [`docs/providers.md`](docs/providers.md)
- [`docs/war-room.md`](docs/war-room.md)
- [`docs/persistence.md`](docs/persistence.md)
- [`warroom/docs/TAURI-AUTH.md`](warroom/docs/TAURI-AUTH.md)

Security and reporting policy are defined by the repository root
[`SECURITY.md`](../SECURITY.md).
