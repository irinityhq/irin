# Arm the Watch Producer

Sentinels observe and record by default. Enabling the producer allows a watch
fire to enter the Gateway dispatcher and may cause paid Council work.

## Before arming

- Keep `WATCH_PRODUCER_ENABLED=false` during setup and Sentinel testing.
- Confirm the selected Sentinel profile and tenant.
- Confirm Gateway, Council, spend limits, and durable state are healthy.
- Run a deterministic Sentinel test without the action path.
- Complete a hardware-backed rehearsal.

The authoritative ceremony is
[Gateway: Arming the Watch Producer](../../../gateway/docs/runbooks/arming-authorization.md).
From the repository root, the rehearsal command is:

```bash
gateway/bin/arm --rehearse
```

Only after the rehearsal and pre-arm checks pass should the operator run
`gateway/bin/arm`. Verify the writer claim, producer status, audit entry, and spend
metrics before creating a test fire.

Do not arm by editing the environment alone. Boot-time environment arming is
a recovery/test surface; the hardware-backed local ceremony is the operator
path.
