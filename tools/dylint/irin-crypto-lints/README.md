# irin_crypto_lints

Workspace-external [Dylint](https://github.com/trailofbits/dylint) library for IRIN crypto invariants.

## Lints

| Lint | Level | What it flags |
| --- | --- | --- |
| `no_debug_on_signing_key_types` | Warn | `#[derive(Debug)]` on types named like `SigningKey` / `SecretKey` / `PrivateKey` / `LedgerKey` / `KeyMaterial`, or with a field type ending in `SigningKey` |
| `prefer_subtle_ct_eq` | Warn | `==` / `!=` between byte-ish values inside `verify`/`auth`/`mac`/… functions; also `#[derive(PartialEq)]` on key-material type names |

## Toolchain

Pinned in `rust-toolchain`:

```text
nightly-2026-04-16  (+ rustc-dev, llvm-tools-preview)
```

Install:

```bash
rustup toolchain install nightly-2026-04-16 -c rustc-dev -c llvm-tools-preview
cargo install cargo-dylint dylint-link --locked
```

## Build & test (this package)

```bash
cd tools/dylint/irin-crypto-lints
cargo build
cargo test
```

UI tests live under `ui/` (`dylint_testing`).

## Run against IRIN workspace crates

From the repo root (advisory wrapper):

```bash
./scripts/run-dylint.sh
# or: make lint-crypto
```

Strict mode (non-zero on findings / tool failure):

```bash
IRIN_DYLINT_FAIL=1 ./scripts/run-dylint.sh
```

Direct `cargo dylint` (after the package builds):

```bash
cargo dylint --path tools/dylint/irin-crypto-lints -- --workspace
```

This package is **not** a member of the root Cargo workspace (`[workspace]` is empty here) so a missing dylint toolchain does not break `make build` / `make test`.
