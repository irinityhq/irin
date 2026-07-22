# Gateway core surfaces

Gateway is opt-in governance for Council seats: metering, auth, budget, cache,
routing, decontamination, policy, ledger, and the watch plane. This page names
load-bearing **non-watch** surfaces that operators hit in code but that used to
live only in the contract or not at all.

See also [`COUNCIL_GATEWAY_CONTRACT.md`](../COUNCIL_GATEWAY_CONTRACT.md) for the
Council↔Gateway wire contract, and [`watch-api.md`](watch-api.md) for Watch.

## Process shape

- **OpenResty** (`:18080` in the canonical stack) accepts HTTP from loopback.
- **Rust sidecar** serves management routes over a **Unix domain socket**
  (default mode `0660`, fail-closed on bad mode/GID).
- Outermost sidecar rate limit defaults to **6000 rpm** (`SIDECAR_GLOBAL_RPM`);
  exhausted → `429` + `Retry-After`. `/health` is exempt.

## Authentication

- Virtual API keys: `SHA-256(AUTH_PEPPER || raw_key)` against `auth_keys.json`.
- `GATEWAY_AUTH_FAIL_CLOSED` defaults **true**; empty key map denies.
- Multi-bucket limits: global, per-IP, per-key; optional CIDR policy.
- Admin provision/revoke: bootstrap token or admin-tier key; no self-revoke.
- Lua front door: if the sidecar is unreachable, auth and IP checks **fail closed**
  (deny), not open.

## Budget

- Per-key default limit **$10 / 24h** unless configured higher/lower per key.
- `/budget/check` pre-flight; `/budget/record` posts actual cost.
- With `GATEWAY_DURABLE=1`, budget/cache use SQLite at `GATEWAY_STATE_DB_PATH`;
  without it, state is in-memory only (data-loss risk across restart).

## Cache and router

- Response cache prefix `gateway:cache:v5:`; hit path can short-circuit Lua.
- Smart router scores models from `models.json` (quality/latency/cost/risk)
  with strategies `quality|balanced|economy|speed`.
- Provider **family** health is isolated so one family's 429s do not darken
  unrelated families on the same provider.

## Guard path

- Decontaminator stages (homoglyph, etc.) from configured JSON.
- Policy firewall classifies sensitivity; treat defaults as fail-safe and
  re-read `policy.rs` / config before asserting production posture.
- Tool enforcer can bound `READ_ONLY` tool use on `/guard/tool`.
- Shape limits (messages/tools) apply at the nginx/Lua edge beyond body size.

## Ledger

- Signed audit ledger for governed calls.
- `GET /ledger/verify` and `GET /ledger/export` require an **admin-tier**
  `X-Admin-Key` (401 missing/invalid, 403 non-admin). Loopback orientation is
  not a substitute for that gate.

## Council helpers on the sidecar

- Concurrency cap for council work (default low, configurable, hard max).
- Durable idempotency claim/store paths with TTLs so client abort does not
  double-bill when configured correctly.
- See contract + `council.rs` when integrating non-War-Room callers.

## Offline ceremony

- `gateway-ceremony` / ledger key tooling supports air-gapped key material
  paths distinct from online `POST /auth/rotate`.

## Operator rule of thumb

Gateway for **metered, authenticated, audited** calls. Watch arming is a
**separate** deliberate act — see
[`runbooks/arming-authorization.md`](runbooks/arming-authorization.md).
