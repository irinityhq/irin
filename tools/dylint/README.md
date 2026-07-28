# IRIN dylint libraries

Custom [Dylint](https://github.com/trailofbits/dylint) libraries used as an **advisory** crypto lint layer for IRIN. They live under `tools/dylint/` and are **not** members of the root Cargo workspace, so a missing nightly / `cargo-dylint` install does not break product builds.

## Packages

| Path | Purpose |
| --- | --- |
| [`irin-crypto-lints/`](irin-crypto-lints/) | Crypto invariants: no `Debug` on key types; prefer CT equality on auth surfaces |

## Operator install

```bash
rustup toolchain install nightly-2026-04-16 -c rustc-dev -c llvm-tools-preview
cargo install cargo-dylint dylint-link --locked
```

## Run (from repo root)

```bash
./scripts/run-dylint.sh
# or: make lint-crypto
```

Default is **advisory** (exit 0 when tools are missing; findings print but do not fail). Set `IRIN_DYLINT_FAIL=1` for gate mode.

See [`irin-crypto-lints/README.md`](irin-crypto-lints/README.md) for lint details and UI tests.
