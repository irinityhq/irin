# IRIN Opengrep rules (Tier-1 security spine)

Local static checks for product-security surfaces: arming, ledger/signing,
auth compares, and Lua credential scrub paths.

## Run

```bash
make tools            # cargo-deny + opengrep into .irin-tools/bin (gitignored)
make lint-security    # advisory scan (exit 0 with findings)
# or:
scripts/run-opengrep.sh
scripts/run-opengrep.sh gateway/sidecar-rs/src/watch/api
IRIN_OPENGREP_FAIL=1 scripts/run-opengrep.sh --fail   # CI-style hard fail
```

JSON + SARIF land under `.irin-tools/findings/` (gitignored). Latest pointers:
`opengrep-latest.json`, `opengrep-latest.sarif`.

**Prefer JSON for paths.** Opengrep's SARIF commonly uses `uriBaseId: %SRCROOT%`
without `originalUriBaseIds`; the runner treats JSON `results[].path` as the
authoritative file location and fails if any finding lacks a path.

## Rules

| ID | Lang | Intent |
| --- | --- | --- |
| `irin.rust.no-eq-on-signature-bytes` | rust | `==`/`!=` on signature/token/secret-like names |
| `irin.rust.eq-on-admin-secret` | rust | plain compare of admin/bootstrap/API keys |
| `irin.rust.no-debug-signing-key` | rust | Debug/format of signing key / seed material |
| `irin.rust.raw-key-bytes-in-string` | rust | hex-encoding private/seed bytes into strings |
| `irin.rust.token-in-log` | rust | token/secret fields in tracing/log macros |
| `irin.rust.arming-without-auth-pattern` | rust | INFO/experimental: arm/disarm without authenticate |
| `irin.rust.arming-bearer-eq` | rust | bearer/token `==` under `watch/` |
| `irin.lua.credential-path` | lua | body/headers to sinks without scrub (best-effort) |
| `irin.lua.ngx-log-authorization` | lua | logging Authorization / admin key headers |
| `irin.rust.guard-scan-requires-debug-env` | rust | `/guard/scan` only under `GATEWAY_DEBUG_GUARD_SCAN` (audit F-1) |
| `irin.rust.uds-router-requires-global-rate-limit` | rust | `build_router` wires `global_rate_limit` (audit F-3) |
| `irin.rust.tenant-policy-write-requires-admin` | rust | `watch_set_tenant_policy` calls `admin_token_matches` (audit F-6) |

Prefer `subtle::ConstantTimeEq` and `admin_token_matches` /
`ArmPrincipals::authenticate` over raw string equality for secrets.

## Notes

- Advisory by default — findings do not fail `make lint-security`.
- Noise is expected on intentional storage of **public** key/signature hex;
  tune rules rather than disabling the scan.
- Binary is checksum-pinned in `scripts/bootstrap-dev-tools.sh` (v1.26.0).
