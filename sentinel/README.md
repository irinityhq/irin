# Sentinel

Sentinel defines IRIN's deterministic watch contract and shared communication
types. Runtime Sentinel implementations are hosted by the Gateway Rust sidecar.

The signal chain is:

```text
Sentinel observes evidence -> Gateway records and routes -> Council deliberates
-> Gateway validates and signs a directive
```

A Sentinel observes state and decides whether evidence is interesting without
calling an LLM. Enabling a Sentinel does not enable the producer or authorize an
action. The supported product path ends at a signed directive. Authenticated
worker-management routes are mounted in Gateway, but the built-in worker loop
is disabled by default and is not an operator-ready autonomous execution path.

## What gets armed

Sentinel itself is not armed: it remains a deterministic observer. The
hardware-backed ceremony arms the **Gateway Watch producer**, which is the seam
that can promote a recorded fire toward the dispatcher, paid Council
deliberation, and a signed Outbox directive.

The canonical runtime loads no Sentinel profile by default — the sidecar boots
healthy with zero Sentinels unless `SENTINELS_CONFIG_PATH` names one — and
leaves both the Gateway Watch dispatcher and producer disabled. These are
separate controls:

- loading or enabling a Sentinel only permits observation and recording;
- enabling the dispatcher can process an already-pending escalation and may
  spend;
- the normal producer path requires an expiring, audited Touch ID or FIDO2
  rehearsal and explicit arm;
- `gateway/bin/disarm` stops new producer claims without waiting for a second
  factor, although already accepted provider work may finish.

Do not “arm” by changing `WATCH_PRODUCER_ENABLED` alone. That boot-time gate is
for recovery and testing; the operator path is the staged hardware ceremony in
[`gateway/docs/runbooks/arming-authorization.md`](../gateway/docs/runbooks/arming-authorization.md).
Even when armed, the supported operator path stops at a signed directive. The
built-in worker loop remains a separate, default-off development path.

The `sovereign-protocol` crate contains shared envelope, directive, canonical
JSON, and validation types used by Council and Gateway.

## Implementation map (for feature work)

- Wire crate: `sovereign-protocol/` (`cargo test -p sovereign-protocol`)
- Runtime hosts of the watch contract: Gateway sidecar (see
  [`gateway/README.md`](../gateway/README.md))
- Capability-token and related execute seams:
  [`../docs/seams/`](../docs/seams/) (start with structured execute authority)

## Documentation

- [`COMMS_CONTRACT.md`](COMMS_CONTRACT.md)
- [`docs/runbooks/arm-producer.md`](docs/runbooks/arm-producer.md)
- [`docs/runbooks/disarm-producer.md`](docs/runbooks/disarm-producer.md)

Report vulnerabilities using the repository root [`SECURITY.md`](../SECURITY.md).
