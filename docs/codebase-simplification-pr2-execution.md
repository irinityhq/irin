# PR 2 Execution Record — Ownership Only

Status: ready for implementation

Branch: `codex/collapse-runtime-ownership-v2`

Base: `acc6237f3f6e5449990759916d704b74b2fa7f59`

Authority: `docs/codebase-simplification-plan.md`, “PR 2 — Collapse runtime
ownership into the DMG.”

## Mission

Collapse Council, War Room web, login recovery, and Settings ownership into the
installed DMG without implementing or migrating governed Claude/Codex CLI
transport.

The packaged app must always own its bundled Council. Source development remains
the foreground `make warroom` process tree. The product must no longer support
MatchingBuild adoption, a source-managed installed runtime, or configurable
Council source paths.

## Required deletion

- MatchingBuild and external-Council adoption.
- External-runtime restart and reconciliation messaging.
- User-facing `councilPath` and `councilRoot`.
- Source-checkout Council path/base-directory overrides that become unreachable.
- Source-managed Council and Next.js lifecycle.
- Login LaunchAgent installation and recovery for that lifecycle.
- Root `setup`, `setup-prepare`, and `runtime-*` product/operator surfaces that
  exist for the retired lifecycle.
- Obsolete tests, documentation, and release wiring for those paths.

Delete complete reachable slices: implementation, tests, documentation, and
entrypoint references together.

## Temporary governed-CLI shim boundary

If the existing Python Claude/Codex proxies must remain temporarily, extract
only an optional launcher that owns those two proxy processes.

The shim must not:

- spawn, stop, inspect, or adopt Council;
- spawn or manage Next.js;
- install or use a login LaunchAgent;
- perform MatchingBuild adoption;
- expose or consume `councilPath` or `councilRoot`;
- become required for installed-app cold start;
- add Rust/native proxy adapters, proxy-token Keychain entries, or a second
  adapter health/restart subsystem.

If the shim needs a substantial fraction of `scripts/irin-runtime.sh`, stop and
report that extraction failed. Do not preserve the old controller under a new
name.

## Explicitly deferred to PR 2a

- Governed CLI architecture selection.
- Native Claude/Codex adapter implementation.
- Proxy-token Keychain migration.
- Docker-to-host proxy redesign.
- Live Claude/Codex provider acceptance.
- macOS provider-CLI Keychain, codesigning, and TCC behavior.

The parked branch `codex/collapse-runtime-ownership` at checkpoint `9cc237c`
is reference material only. Do not cherry-pick its native adapter or Keychain
work into this branch.

## Proof

- Reference search shows no supported source Council/Next/login/adoption
  lifecycle.
- Root help advertises foreground `make warroom` and the DMG product path.
- Packaged cold start owns bundled Council without a checkout or source runtime.
- Closing the app terminates only its owned child.
- Foreground browser development remains usable.
- Tailscale publication remains app-owned, port-scoped, and never enables
  Funnel or resets unrelated Serve configuration.
- Any temporary governed-CLI shim is optional and owns only the existing proxy
  pair.
- Proportionate Tauri, War Room/export, Gateway Pack, release-tree, and packaged
  smoke checks pass without live provider spend.

## Stop conditions

Stop and report rather than expanding scope if:

- ownership deletion requires new provider transport;
- the proxy shim begins acquiring Council, web, login, adoption, or Settings
  responsibilities;
- a proposed replacement adds more lifecycle code than it deletes;
- deterministic or packaged proof reveals loss of Direct CLI, War Room,
  Gateway Pack, or Tailscale behavior;
- completing the task would require a live provider call.

Do not commit, push, open a PR, install a release, or delete operator runtime
state during the implementation dispatch.
