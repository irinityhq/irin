# Security tooling (Tier 1, local-first)

Local scanners and formal checks that complement the existing CI supply-chain
gates (`cargo-deny`, `cargo-audit`, gitleaks, CodeQL). **Authoritative finding
shape is file + line** (Opengrep JSON under `.irin-tools/findings/`). There is
no SARIF→Gortex symbol merger in-tree: Gortex remains the structural spine for
agents; scanners stay separate tools agents invoke on paths Gortex selects.

## What is already in CI

| Tool | Role |
| --- | --- |
| `cargo-deny` + `cargo-audit` | Advisories, licenses, banned sources |
| gitleaks | Secret scan |
| CodeQL | OSS language SAST (not the local custom-rule path) |
| Opengrep + Selene | **Advisory** job `security-scanners` in `ci.yml`: same scripts as `make lint-security` / `make lint-lua` (no `IRIN_*_FAIL`). Findings do not fail the job; bootstrap/tool/config errors do. Opengrep JSON/SARIF uploaded as the `security-scanner-findings` artifact. |

## Local / gitignored state

Pinned binaries and scan artifacts live under **`.irin-tools/`** (gitignored):

```text
.irin-tools/bin/opengrep      # bootstrap via make tools
.irin-tools/bin/cargo-deny
.irin-tools/bin/selene        # Selene (gateway Lua)
.irin-tools/findings/*.json   # Opengrep outputs (paths are authoritative)
.irin-tools/findings/*.sarif  # optional; prefer JSON for path reliability
```

Never force-add this directory. Operator toolchains (kani, miri, cargo-dylint)
install into the user cargo/rustup homes — also not committed.

## Install

```bash
make tools                 # cargo-deny + opengrep + selene (+ actionlint) into .irin-tools/
# Optional deeper tools (operator machine):
cargo install cargo-dylint dylint-link kani-verifier --locked
cargo kani setup
rustup toolchain install nightly -c miri
# dylint library needs a matching nightly; see tools/dylint/README.md
```

## Run (advisory by default)

Local and CI use the same runners. CI bootstraps via `scripts/bootstrap-dev-tools.sh`
then calls `scripts/run-opengrep.sh` and `scripts/run-selene.sh` without fail-closed env.

```bash
make lint-security         # Opengrep IRIN rules → .irin-tools/findings/
make lint-lua              # Selene on gateway/lua (OpenResty std)
make lint-crypto           # dylint IRIN crypto lints
make verify-formal         # selective Kani + Miri on sovereign-protocol JCS
```

Fail-closed (opt-in):

```bash
IRIN_OPENGREP_FAIL=1 make lint-security
IRIN_SELENE_FAIL=1 make lint-lua
IRIN_DYLINT_FAIL=1 make lint-crypto
IRIN_KANI_FAIL=1 IRIN_MIRI_FAIL=1 make verify-formal
```

## Agent workflow

1. Gortex: `detect` / impact on the change set (structure only).
2. If signing, arming, budget, redaction, or JCS paths move → `make lint-security`
   (or `scripts/run-opengrep.sh <paths>`). Read **JSON** findings for file:line.
3. Key-type / compare changes → `make lint-crypto`.
4. Pure JCS / fail-closed helpers in `sovereign-protocol` → update Kani harnesses
   under `src/jcs/kani_proofs.rs`, then `make verify-formal`.

## Config-key blast radius (convention, not a graph)

Env/config keys are unique strings. When a key is added, renamed, or removed,
its readers are one search away — no graph build, no emitter:

```bash
rg -n 'BOOTSTRAP_TOKEN'
```

Search everything, then classify the hits: providers live in env examples,
compose files, and setup scripts, not only in `*.rs` / `*.lua` / `*.ts`
sources. Every hit is a reader or provider of that key; that is the blast
radius.
Gortex's `config_readers` / `env_var_users` kinds currently emit config edges
for Go-shaped code only, so they return empty on this Rust/Lua-heavy tree —
expected, not misconfiguration. `analyze contracts` still surfaces env
contracts from docker-compose and TS `process.env` reads. If a real incident
shows search-level lookup was insufficient, that receipt reopens the
conversation; do not build a config-edge emitter on speculation.

## Rules and lints (committed)

| Path | Content |
| --- | --- |
| `security/opengrep/rules/` | IRIN Opengrep YAML (crypto/arming/Lua) |
| `security/selene/std/openresty.yml` | OpenResty `ngx` std for Selene (`std = lua51+…`) |
| `selene.toml` | Selene config (exclude + baseline rule severities) |
| `tools/dylint/irin-crypto-lints/` | Custom dylint library (external workspace) |
| `sentinel/sovereign-protocol/src/jcs/kani_proofs.rs` | Selective formal proofs |

### Selene (gateway Lua)

| | |
| --- | --- |
| Covers | Static lint of `gateway/lua` (14 OpenResty Lua files): undefined globals, style, unused bindings, with an IRIN OpenResty std for `ngx.*` used in-tree |
| Status | **Advisory** by default (`make lint-lua` exits 0 with findings) |
| Run | `make tools` once, then `make lint-lua` or `scripts/run-selene.sh [PATH…]` |
| Hard mode | `IRIN_SELENE_FAIL=1 make lint-lua` or `scripts/run-selene.sh --fail` |
| Missing binary | Runner prints a bootstrap hint and exits 0 (does not auto-install) |

## Explicit non-goals

- SARIF→Gortex symbol attribution (needs a real range-to-symbol API + fixtures)
- Lockbud, loom/shuttle, KCL migration, Structurizr
- Hard-fail ship-check on all scanners before baselines settle
