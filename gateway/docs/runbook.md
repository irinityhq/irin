# Gateway Operator Runbook

This runbook covers the canonical local Gateway started by the macOS root IRIN
runtime controller. Ubuntu supports Gateway's Docker/component and verification
paths, but not this `launchctl`-based managed controller. This runbook assumes
a single operator and keeps all secrets and mutable state outside the
repository.

## Local layout

| Item | Default location |
|---|---|
| Gateway environment | `~/.config/irin/gateway.env` |
| Provider credentials | Login-shell environment |
| Ledger signing key | `~/.irin/ledger_key.pem` |
| Gateway state | Docker volume `gateway_sidecar_data` |
| Gateway URL | `http://127.0.0.1:18080` |

From the IRIN repository root, run `make setup-prepare` to prepare local files
without starting the product. Setup preserves valid operator-owned values while
adding or replacing missing, placeholder, or invalid IRIN-managed fields. The
Gateway environment and signing key must remain mode `0600`; provider keys are
not copied into either.

## Start, stop, and inspect

From the repository root:

```bash
make runtime-up
make runtime-status
make runtime-down
```

The runtime controller builds Council, War Room Web, Gateway, and the Rust
sidecar from the same checkout. It uses Docker Compose project `gateway` so an
existing `gateway_sidecar_data` volume is reused.

Useful checks:

```bash
curl -fsS http://127.0.0.1:18080/health
docker compose -p gateway \
  --env-file "$HOME/.config/irin/gateway.env" \
  -f gateway/docker-compose.yml \
  -f gateway/docker-compose.canary.yml ps
docker compose -p gateway logs --tail=100 sidecar
```

Do not use `docker compose down -v` on the canonical runtime. The `-v` flag
deletes durable Gateway state.

## Bootstrap an operator key

`make setup-prepare` creates a one-time `BOOTSTRAP_TOKEN`. Use it only when no
admin key exists:

```bash
set -a
. "$HOME/.config/irin/gateway.env"
set +a

curl -fsS -X POST http://127.0.0.1:18080/admin/keys \
  -H 'Content-Type: application/json' \
  -d "{\"budget_key\":\"operator\",\"tier\":\"admin\",\"rpm\":1000,\"admin_key\":\"$BOOTSTRAP_TOKEN\"}"
```

The raw key is returned once. Store it outside the repository. Before retiring
the one-time bootstrap credential, ensure the private runtime environment has a
non-empty `WATCH_ADMIN_TOKEN`; canonical `make setup` generates this separate
credential automatically. After at least two admin keys exist, remove
`BOOTSTRAP_TOKEN` and restart the runtime. Watch and Outbox admin reads continue
to use `WATCH_ADMIN_TOKEN`. Older installs should rerun `make setup-prepare` to
add the missing value without replacing or printing existing credentials.

Create another key with an existing admin key:

```bash
ADMIN_KEY='gw_live_...' make -C gateway provision-key \
  BUDGET=operator-backup TIER=admin RPM=1000
```

## Revoke a virtual key

Revoke by key ID so the raw key does not enter shell history:

```bash
curl -fsS -X POST http://127.0.0.1:18080/admin/keys/revoke \
  -H 'Content-Type: application/json' \
  -d "{\"key_id\":\"k_example\",\"admin_key\":\"$ADMIN_KEY\"}"
```

Revocation takes effect on the next request. An admin cannot revoke itself;
use the second admin key.

## Verify the ledger

```bash
LEDGER_ADMIN_KEY="$ADMIN_KEY" make -C gateway ledger-verify
make -C gateway ledger-fsck
```

The signing key is exactly 32 bytes. Never replace it in place while the
sidecar is running. A signing-key rotation must keep the prior key available
for verification until the intended retention window has elapsed.

## Sentinel configuration

The canonical local stack mounts
`gateway/test/fixtures/canary-sentinels.yaml`. It is a validated development
profile, not permission to enable action. The dispatcher and producer remain
independent gates:

```text
WATCH_DISPATCHER_ENABLED=false
WATCH_PRODUCER_ENABLED=false
```

Use the read-only Watch surface to inspect registered sentinels and fires.
Follow [Arming the watch producer](runbooks/arming-authorization.md) before
enabling any action path.

## Backup and recovery

Stop the runtime before taking a state backup:

```bash
make runtime-down
docker run --rm \
  -v gateway_sidecar_data:/source:ro \
  -v "$PWD":/backup \
  alpine:3.21 tar -C /source -czf /backup/gateway-state.tgz .
```

Store the archive and signing key separately. A useful restore requires both
the state volume and the corresponding verification key material.

## Failure guide

| Symptom | First check | Safe response |
|---|---|---|
| `/health` is unavailable | `docker compose -p gateway ps` | Inspect sidecar and OpenResty logs; do not delete volumes. |
| Sidecar rejects the signing key | file size and mode | Restore the intended 32-byte `0600` key; do not generate over it. |
| Watch producer self-disarms | writer claim, spend cap, database health | Leave it disarmed, collect logs, and resolve the cause before rehearsal. |
| Council calls fail | Council `:8765` health and `COUNCIL_GATEWAY_TOKEN` | Restore matching local config, then restart. |
| Tailscale route is unavailable | `tailscale status` and `tailscale serve status` | Keep local services bound to loopback; repair Tailscale separately. |

The isolated no-spend integration proof is `make verify`. It uses separate
ports, project name, and state and does not touch the canonical runtime.
