# Security tooling (Gortex spine + Tier 1)

Local-first tools that fill gaps around Gortex. Agents still start with Gortex
(`detect_changes` / impact / editing context). Deeper scanners run on the
affected subgraph; findings join back to Gortex symbols via the merger.

## What is already in CI

| Tool | Role |
| --- | --- |
| `cargo-deny` + `cargo-audit` | Advisories, licenses, banned sources |
| gitleaks | Secret scan |
| CodeQL | OSS language SAST (keep; not the local custom-rule path) |

## Local / gitignored state

Pinned binaries and scan artifacts live under **`.irin-tools/`** (gitignored):

```text
.irin-tools/bin/opengrep      # bootstrap via make tools
.irin-tools/bin/cargo-deny
.irin-tools/findings/*.json   # Opengrep outputs
.irin-tools/findings/*.sarif
.irin-tools/findings/merged-*.jsonl
```

Never force-add this directory. Operator machine toolchains (kani, miri,
cargo-dylint, selene) install into the user cargo/rustup homes — also not
committed.

## Install

```bash
make tools                 # cargo-deny + opengrep (+ actionlint) into .irin-tools/
# Optional deeper tools (operator machine):
cargo install cargo-dylint dylint-link kani-verifier --locked
cargo kani setup
rustup toolchain install nightly -c miri
# dylint library needs a matching nightly; see tools/dylint/README.md
```

## Run (advisory by default)

```bash
make lint-security         # Opengrep IRIN rules → .irin-tools/findings/
make lint-crypto           # dylint IRIN crypto lints
make verify-formal         # selective Kani + Miri on sovereign-protocol JCS

# Map SARIF onto Gortex symbols (JSONL stays under .irin-tools/findings/)
python3 scripts/merge-findings-to-gortex.py .irin-tools/findings/<latest>.sarif
```

Fail-closed (opt-in):

```bash
IRIN_OPENGREP_FAIL=1 make lint-security
IRIN_DYLINT_FAIL=1 make lint-crypto
IRIN_KANI_FAIL=1 IRIN_MIRI_FAIL=1 make verify-formal
```

## Agent workflow

1. Gortex: `detect` / impact on the change set.
2. If signing, arming, budget, redaction, or JCS paths move → `make lint-security`
   (or `scripts/run-opengrep.sh <paths>`).
3. Key-type / compare changes → `make lint-crypto`.
4. Pure JCS / fail-closed helpers in `sovereign-protocol` → update Kani harnesses
   under `src/jcs/kani_proofs.rs`, then `make verify-formal`.
5. Merge SARIF and query findings by symbol when blast-radius questions need
   security edges.

## Rules and lints (committed)

| Path | Content |
| --- | --- |
| `security/opengrep/rules/` | IRIN Opengrep YAML (taint/crypto/arming/Lua) |
| `tools/dylint/irin-crypto-lints/` | Custom dylint library (external workspace) |
| `sentinel/sovereign-protocol/src/jcs/kani_proofs.rs` | Selective formal proofs |

## Explicit non-goals (Tier 3 / later)

Lockbud, loom/shuttle, KCL migration, Structurizr, dual graph SSOT, hard-fail
ship-check on all scanners before baselines settle.
