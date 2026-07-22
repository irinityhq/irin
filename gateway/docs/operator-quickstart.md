# Gateway Operator Quickstart

On macOS, Gateway is normally started with the complete IRIN runtime from the
repository root:

```bash
make setup
make runtime-status
```

The default endpoint is `http://127.0.0.1:18080`. Do not bind Gateway directly
to an untrusted network.

Ubuntu runs Gateway through its Docker/component paths rather than the
macOS-only root runtime controller. Start with [`verify.md`](verify.md) for the
portable isolated lane and [`runbook.md`](runbook.md) for the canonical
operator boundary; `make warroom` alone starts Council and War Room Web, not
Gateway.

## Local Configuration

The root setup creates:

- `~/.config/irin/gateway.env` for Gateway and runtime settings
- `~/.irin/ledger_key.pem` for the local Ed25519 signing seed

Configuration files and the signing seed are mode `0600`. Setup preserves
valid operator-owned values while adding or replacing missing, placeholder, or
invalid IRIN-managed fields. Provider credentials remain in the login-shell
environment and are never copied into Gateway configuration.

## Health

```bash
curl -fsS http://127.0.0.1:18080/health
curl -fsS http://127.0.0.1:18080/metrics | head
make runtime-status
```

Gateway is fail-closed. A healthy service can still return `401` for protected
routes until a caller key is provisioned.

## Initial Admin Key

The first admin key is created with the one-time `BOOTSTRAP_TOKEN` stored in the
private Gateway environment file:

```bash
BOOTSTRAP_TOKEN=<local-bootstrap-token> \
  make -C gateway provision-key BUDGET=ops TIER=admin
```

The response displays the raw caller key once. Store it in a private operator
credential store. Subsequent key creation should use an existing admin key.
See [`runbook.md`](runbook.md) for rotation and recovery.

## Sentinels

The runtime loads Sentinel definitions from the file selected by
`SENTINELS_CONFIG_PATH`. The canonical local profile mounts
`test/fixtures/canary-sentinels.yaml`, which contains one deterministic
file-inbox watch. Action production remains disabled by default.

Inspect the loaded registry in the sidecar logs:

```bash
docker logs gateway-sidecar-1 2>&1 | grep 'sentinels.yaml: loaded'
```

Additional stock types are implemented under
`sidecar-rs/src/watch/sentinels/`. Test fixtures for anomaly, ledger delta, and
the dispatcher smoke live under `test/fixtures/`. Enabling a Sentinel does not
enable `WATCH_PRODUCER_ENABLED` and does not arm an action path.

## Producer states and arming

“Arming Gateway” means arming the Gateway **Watch producer**, not starting the
Gateway service and not enabling Governed provider routing. The controls are
separate because each changes a different part of the lane:

| State/control | What it permits |
| --- | --- |
| Sentinel definition loaded | Observe and record matching evidence only. |
| Dispatcher enabled | Claim already-pending escalations and invoke Council; this may spend. |
| Producer boot gate enabled | Permit the producer to promote Watch fires after all boot-time live-mode/key checks pass; this is a recovery/test surface, not the normal operator ceremony. |
| Hardware-backed producer arm | Start the producer through the staged, expiring, audited Touch ID/FIDO2 ceremony. |
| Disarm | Stop new producer claims; already accepted provider work may finish. |

The canonical runtime starts with both `WATCH_DISPATCHER_ENABLED=false` and
`WATCH_PRODUCER_ENABLED=false`. Before changing either, configure the intended
tenant, dispatcher credential, Council reachability, and spend ceilings. Then
follow the authoritative
[`arming-authorization.md`](runbooks/arming-authorization.md): enroll a hardware
credential, run `gateway/bin/arm --rehearse`, inspect the result, and only then
run `gateway/bin/arm`. The immediate kill switch is `gateway/bin/disarm` and
does not require a second factor.

Arming can produce paid deliberation and a signed directive. It cannot cause an
external autonomous action; the authenticated worker-management routes do not
make the separate, default-off built-in worker loop an operator-ready path.

Use the deterministic integration harness before a live test:

```bash
make -C gateway smoke-phase3
```

The live variant can call providers and must be run deliberately:

```bash
PHASE3_SMOKE_COUNCIL_MODE=live \
PHASE3_SMOKE_TRIGGER_MODE=file-inbox \
make -C gateway smoke-phase3
```

## CLI Provider Proxies

Gateway includes optional host-side proxies for authenticated Claude, Codex,
and Gemini CLIs under `tools/`. The canonical IRIN runtime automatically starts
the Claude and Codex proxies when their CLIs are present. They bind all host
interfaces so Docker Desktop can reach them, and refuse that bind unless setup's
distinct shared proxy token is available. Do not forward these ports or expose
them outside a trusted private host/network boundary. Standalone operators who
launch a proxy manually should keep its default loopback bind unless container
access is required and authenticated.

## Day-2 Commands

From the repository root:

```bash
make runtime-status
make runtime-restart
make runtime-down
make verify
make verify-down
```

For signing, key management, failure recovery, and database inspection, use
[`runbook.md`](runbook.md). For arming, use
[`runbooks/arming-authorization.md`](runbooks/arming-authorization.md).
