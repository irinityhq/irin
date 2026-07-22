# Gateway

Gateway is IRIN's local routing, metering, watch, and signing plane. OpenResty
accepts caller requests and communicates with the Rust sidecar over a Unix
domain socket. The sidecar owns authentication, durable watch state, budget
checks, the signed directive outbox, and arming controls.

From the IRIN repository root, the canonical runtime starts Gateway on
`127.0.0.1:18080`:

```bash
make runtime-up
make runtime-status
```

From that same root, the isolated no-key proof is:

```bash
make verify
make verify-down
```

## What “arming Gateway” means

Gateway itself does **not** need to be armed for ordinary Direct or Governed
Council calls. The armable surface is the **Watch producer**: the Gateway
sidecar process that can promote a recorded Sentinel fire into the dispatcher,
potentially causing paid Council work and a signed Outbox directive.

The default runtime is deliberately inert: one test Sentinel is loaded, while
the dispatcher and producer are both disabled. Loading a Sentinel definition,
enabling the dispatcher, enabling the producer's boot gate, and completing the
hardware-backed producer ceremony are distinct controls. Do not treat any one
flag as permission to activate the complete lane:

```text
Sentinel observes -> Watch records -> producer promotes -> dispatcher invokes Council
    -> Gateway validates and signs an Outbox directive
```

The operator path is: verify the selected Sentinel/tenant, Council health,
dispatcher credential, and spend ceilings; enroll Touch ID or FIDO2; run a
rehearsal; then explicitly arm with `gateway/bin/arm`. Use
`gateway/bin/disarm` as the immediate kill switch. Disarm stops new claims but
cannot cancel provider work already in flight. Authenticated worker-management
routes are mounted, but the built-in worker loop is disabled by default;
autonomous execution beyond the signed directive is not an operator-ready
product path.

Useful operator documentation:

- [`docs/operator-quickstart.md`](docs/operator-quickstart.md)
- [`docs/runbook.md`](docs/runbook.md)
- [`docs/verify.md`](docs/verify.md)
- [`docs/runbooks/arming-authorization.md`](docs/runbooks/arming-authorization.md)
- [`docs/watch-plane-retention.md`](docs/watch-plane-retention.md)
- [`COUNCIL_GATEWAY_CONTRACT.md`](COUNCIL_GATEWAY_CONTRACT.md)

Gateway is fail-closed when caller credentials are absent. Enabling a Sentinel
does not enable the watch producer or arm an action path. Report vulnerabilities
using the repository root [`SECURITY.md`](../SECURITY.md).
