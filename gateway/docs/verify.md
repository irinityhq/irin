# The verify path — fresh clone to a signed directive in one command

`make verify` proves the whole Sovereign Triad signal chain end to end, from a
fresh clone, with **no real provider keys and no Touch-ID hardware arm**:

```
watch_fires (seeded) → CDC producer → pending_escalation → dispatcher claims →
gateway routes `model: council-triage` → a local no-spend deliberation
endpoint returns a deterministic directive ($0 — no model is called) → the
sidecar signs it (Ed25519 over RFC 8785 JCS) into directive_outbox
```

...and prints the signed row plus elapsed time.

## Run it

```bash
# From the IRIN repository root
make verify        # bring the isolated stack up and prove the closed loop
make verify-down   # tear it all down (stack + volumes + generated secrets)
```

That is the entire public path. No editing `.env`, no provisioning keys by
hand, no hardware enrollment.

Internal machinery names stay unchanged: `test/demo.sh`,
`docker-compose.demo.yml`, compose project `irin-demo`, and the
`DEMO_GW_PORT`/`DEMO_COUNCIL_PORT`/`DEMO_ALLOW_BUILD` env vars are implementation
details, not the public command names.

## What it does (and does not do) for you

- **Generates throwaway dev secrets** (`openssl rand`) into `.env.demo` — it
  **never writes or reads `.env`**, so it cannot clobber a real config.
- **Runs fully isolated**: its own compose project (`irin-demo`), its own host
  ports (gateway `28080`, local deliberation endpoint `28765` — never `18080`), its own named
  volumes, and its **own ephemeral ledger key** under `.demo-state/` (never your
  `~/.irin/ledger_key.pem`). It is safe to run on a machine that is already
  running `make -C gateway up` or a live/canary stack — it can never
  bind-conflict with or
  share state with it.
- **Reuses an existing local sidecar/gateway image built from this tip**; it never
  builds unattended and **does not pull Hub images by default**. If no local
  image is present it STOPS and reports rather than starting a slow cargo build
  behind your back (OOM risk on a shared daemon) or silently running published
  tags that can lag this git tip. On a clean machine you own, build once with
  `DEMO_ALLOW_BUILD=1 make verify` (~8GB Docker). That is the **exact-source**
  path. Opt-in Hub pull (`DEMO_PULL=1`) may lag the tip — do not use it when you
  need source↔binary provenance. The `<120s` clock starts *after* image
  availability (the one-time build is excluded from the runtime measurement).
- **Substitutes the two things a public verification path must not require**:
  the Council (a local `$0` HTTP endpoint returns a deterministic
  `irin.directive.proposal.v1` Act directive — no model is ever called) and
  the arm (a **software** P-256 key generated on the fly, the same mechanism
  CI uses — *not* a real Secure-Enclave / Touch-ID arm). Everything else —
  the watch plane, the CDC producer, the dispatcher, the Ed25519/JCS signing,
  the outbox — is the real production code path.

## Knobs

| Env | Default | Purpose |
|---|---|---|
| `DEMO_GW_PORT` | `28080` | Gateway host port for the isolated verification stack |
| `DEMO_COUNCIL_PORT` | `28765` | Local no-spend deliberation endpoint port |
| `DEMO_POLL_TIMEOUT` | `90` | Seconds to wait for the closed loop |
| `DEMO_ALLOW_BUILD` | `0` | Set `1` on a machine you own to build images from **this checkout** (exact source; ~8GB Docker) |
| `DEMO_PULL` | `0` | Set `1` only to pull published Hub tags; **may lag this git tip** — not the provenance path |

## Requirements

`git`, `make` (stock Ubuntu server ships without it: `apt install make`),
`docker` + `docker compose` v2, `openssl`, `python3` (with the stdlib; the
optional gateway-surface Ed25519 re-verification also uses the `cryptography`
package if it is importable), and `jq`. The reused image already contains
`sqlite3`.

Building the images yourself (`DEMO_ALLOW_BUILD=1`) additionally requires
**BuildKit** (`docker buildx`) — included in Docker Desktop and docker-ce; on
Debian/Ubuntu `docker.io` installs, add the `docker-buildx` package first or
the sidecar image build fails with "the classic builder doesn't support
additional contexts".

## This is the PUBLIC leg only

The verify path's arm uses a software key. The **armed / Touch-ID execution** path — compiling
`bin/arm-attest.swift`, enrolling a Secure-Enclave key, mapping principals — is
a documented **Day-2 operator** setup, not this fresh-clone path. See
[`docs/runbooks/arming-authorization.md`](runbooks/arming-authorization.md) and
[`operator-quickstart.md`](operator-quickstart.md).

## Troubleshooting

- **`arm STAGE returned rehearsal=true`** — the reused sidecar image is a
  *dirty* build and refuses to real-arm (by design). Rebuild a clean image
  (`docker image rm gateway-sidecar && DEMO_ALLOW_BUILD=1 make verify`) or run
  from a clean checkout.
- **`required image(s) missing`** — by design: default path reuses local images
  only (`DEMO_PULL=0`, no unattended build). On a clean machine:
  `DEMO_ALLOW_BUILD=1 make verify` (exact source, ~8GB Docker). If you
  intentionally accept possible Hub lag: `DEMO_PULL=1 make verify`.
- **A step hangs or the loop times out** — inspect the stack (it is left up on
  failure): `docker compose -p irin-demo -f gateway/docker-compose.yml -f
  gateway/docker-compose.demo.yml logs --tail=80 sidecar`, then `make
  verify-down`.
- **Port already in use** — set `DEMO_GW_PORT` / `DEMO_COUNCIL_PORT` to free
  ports.
