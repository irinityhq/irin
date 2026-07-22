# Agent Guide to IRIN

This guide is for a coding or operations agent working with an IRIN checkout.
IRIN combines Council, Gateway, the War Room, and deterministic Sentinels in one
repository.

## Start Safely

On macOS, use the managed repository-root runtime:

```bash
make setup
make runtime-status
```

On Ubuntu, use `make warroom` for Council and the browser War Room. Use
`make verify` for the isolated Sentinel-to-signed-directive proof; the managed
`make setup`/`make runtime-*` controller is currently macOS-only.

The canonical runtime binds to loopback. Tailscale access is an optional private
overlay controlled by the operator. Do not expose the services directly to a
public network.

For a no-provider, no-key behavioral proof:

```bash
make verify
make verify-down
```

The verification stack is disposable and separate from the canonical runtime.

## Know the Boundaries

- Sentinel observes deterministic evidence and does not call an LLM.
- Gateway authenticates, meters, records, routes, and signs.
- Council deliberates and returns a proposed directive.
- The supported product path stops at a signed directive artifact.
- Authenticated worker-management routes are mounted in Gateway, but the
  built-in worker loop is disabled by default and is not an operator-ready
  autonomous execution path.

Do not describe a signed directive as an action that already happened.

## Protect the Running Product

The canonical runtime checkout is deployment-only. Create a Git worktree from
`origin/main` for every task that changes files. A change spanning Council,
Gateway, and Sentinel remains one branch and one pull request.

```bash
make worktree BRANCH=feature/example
```

Worktree tests must use separate ports, Docker project names, volumes, and test
keys. They must not alter the canonical Tailscale route or durable Gateway
volume.

## Spend and Action Safety

- Provider smoke calls and live deliberations can incur cost.
- `make verify` uses a deterministic no-spend endpoint.
- Enabling a Sentinel does not enable the producer.
- Do not set `WATCH_PRODUCER_ENABLED=true` or arm an action path without an
  explicit operator request.
- Use `docs/runbooks/arm-producer.md` and `disarm-producer.md` for those paths.

## Secrets

Credentials and signing material live under `~/.config/irin/`, `~/.irin/`, and
operator-managed credential stores. Never stage environment files, session
records, generated indexes, logs, or Docker state.

## Claims

State only what a command proves. `make runtime-status` proves liveness of the
configured local services. `make verify` proves the isolated signed-directive
path. Neither proves a live provider call, an armed producer, or Worker
execution.

Read these files before changing a wire or security boundary:

- `SECURITY.md`
- `docs/security-claims-vs-reality.md`
- `sentinel/COMMS_CONTRACT.md`
- `gateway/COUNCIL_GATEWAY_CONTRACT.md`
