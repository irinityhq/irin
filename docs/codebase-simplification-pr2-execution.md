# PR 2 Execution Record — Collapse Runtime Ownership into the DMG

Status: consolidated checkpoint; installed-app acceptance pending

Branch: `codex/collapse-runtime-ownership-v2`

Base: `acc6237f3f6e5449990759916d704b74b2fa7f59`

Authority: `docs/codebase-simplification-plan.md`, "PR 2 — Collapse runtime
ownership into the DMG."

## Consolidation note

This branch previously attempted an ownership-only PR 2 that deferred governed
Claude/Codex CLI transport to a "PR 2a" behind a temporary shell shim. That
split was abandoned: the simplification plan makes governed CLI routing a
blocking migration, not an accepted deletion, so the ownership collapse and the
adapter migration ship as one PR 2. The native adapter and Keychain work from
`codex/collapse-runtime-ownership` (`9cc237c`, plus the Keychain-prompt dedup
fix `1cdb670`) is merged into this branch, and the shim
(`scripts/governed-cli-proxies.sh`) is removed.

## Objective

Make the installed `IRIN.app` the sole owner of its bundled Council and optional
Gateway Pack while preserving governed Claude and Codex CLI routing. Remove the
source-managed runtime only after the installed route is proven.

The packaged app must always own its bundled Council. Source development remains
the foreground `make warroom` process tree. The product must no longer support
MatchingBuild adoption, a source-managed installed runtime, or configurable
Council source paths.

## Implemented checkpoint

- Native Claude and Codex CLI adapters are owned by the Tauri Gateway Pack
  lifecycle.
- Proxy endpoints and tokens are injected through the app-owned path without
  exposing secret values.
- Governed routes fail closed; Direct CLI remains distinct and independently
  selectable.
- Native adapters retain the prior per-IP token-bucket behavior: burst 5,
  sustained 10 requests per minute, with bounded cleanup and HTTP 429 on
  exhaustion.
- Gateway Pack resume/start work is single-flight and bounded.
- Packaged War Room boot/discovery recovery is present.
- Source-managed runtime retired: `scripts/setup-local.sh`,
  `scripts/irin-runtime.sh`, their dedicated tests, root `setup` /
  `setup-prepare` / `runtime-*` targets, login-recovery installation, and
  MatchingBuild/external-Council adoption are deleted.
- User-facing `councilPath` / `councilRoot` removed from the Settings/config
  contract and Tauri command arguments; fixed test injection seams retained
  only where automated tests require them.
- Keychain proxy tokens are read once per enable flight and threaded through,
  so a cold launch does not stack per-account authorization dialogs.

## Verified at this checkpoint

- Tauri library tests: 274 passed, 0 failed on the exact auth-observation-cache
  working tree on 2026-07-30. The run exposed one test-only helper as dead code
  in production; it is now gated to test builds and must be rechecked by the
  final Clippy/ship gate.
- `git diff --check`: clean before this execution-record refresh; re-check with
  the final exact-tree gates below.
- Fresh DMG build completed on 2026-07-29 (pre-consolidation); a fresh exact-tree
  build and verification remain required.

These are source/build facts, not installed-app acceptance.

## Known regressions under repair

- War Room Outbox and Watch tabs returned 503 in the installed app because the
  owned Council child was spawned without its governance client configuration
  (`GovernanceClient::from_env` fails, Council answers 503). The pack
  watch-admin read surface is now re-armed: a Keychain-held `WATCH_ADMIN_TOKEN`
  is minted at Gateway Pack Enable, admitted through the validated compose
  secret env into the sidecar container, and re-injected into the governed
  Council child after the gateway spawn scrub. The value is never written to
  the public env file, and ambient host values stay scrubbed; Watch
  producer/dispatcher and the Council-spend route remain force-disarmed.
- `grok_build` seat unavailable: Grok Build CLI detection fingerprints
  `--version` output against a moving upstream format. Fingerprinting is being
  removed in favor of plain binary resolution (`COUNCIL_GROK_CLI_BIN` override,
  then PATH).

## Auth-observation cache (status-loop Keychain bounding)

The enable/resume flight was closed to one Keychain get per account by the
`b3afad6` work (proven by `full_start_resume_keychain_gets_each_account_at_most_once`).
Two repeat sources remained:

1. **Status background loop** — `gateway_pack_status_uncached` called
   `load_gw_api_key` on every probe. The background tick is 5s and the
   presentation `STATUS_CACHE` TTL is 2s, so the keychain was re-read ~12×/min
   indefinitely while the app was open. Under the signed DMG's ACL policy each
   read can surface a macOS authorization dialog.
2. **Governed-spawn double read** — `lib.rs` loaded `GW_API_KEY` for the child
   credentials, then called `gateway_pack_status_fresh` which re-read the same
   account — one redundant get within a single spawn flight.

### Fix

- A **derived auth-observation cache** (`AUTH_OBSERVATION`, lifecycle-invalidated,
  generation-guarded) memoizes `{ key_present, authenticated }` — never the raw
  key value. The background presentation path (`gateway_pack_status`) serves from
  cache on a hit; on a miss it probes once (one Keychain get + one `/v1/models`).
  Once committed, the observation persists **indefinitely** until
  `invalidate_auth_observation()` fires — no TTL. This bounds background reads to
  one per first-access, then zero until enable/disable/uninstall.
- **Authority paths bypass the cache entirely.** `gateway_pack_status_fresh`
  loads `GW_API_KEY` once itself and threads it through the probe, so Touch ID
  arm/renew (`gateway_ready_for_arm`), governed spawn, and post-lifecycle checks
  always run `/v1/models` live — a cached `true` can never permit the ceremony
  after auth has failed, and a cached `false` can never reject a recovered
  Gateway. `gateway_pack_status_fresh_with_key` lets a caller that already holds
  the key (governed spawn at `lib.rs:484`, enable at `enable.rs:336`) thread it
  without a redundant get.
- **Dedicated invalidation.** `invalidate_auth_observation()` is called only at
  genuine credential/lifecycle mutations (enable/disable/stop/uninstall in
  `enable.rs`, post-`migrate_legacy_secrets` at startup) — not on every fresh
  status sample or route transition, which fire far more often than the key
  actually changes.
- **Generation guard.** A probe that read an older generation cannot commit a
  stale observation over a newer one (the key may have been rotated
  mid-probe by an interleaving lifecycle mutation).

### Tests (source proof only)

Eleven auth-observation tests in `status_tests.rs` prove the contract boundary:
cache-hit avoids a second read, cached absence is not re-probed, invalidation
forces a re-read, the held-key path skips Keychain entirely, stale-generation
commit is rejected, concurrent generations cannot double-write, the authority
path ignores a warm cached-true observation, background reads stay constant
across 20 calls until explicit invalidation, a stale generation cannot
repopulate the observation after invalidation, and absent or unreadable
Keychain authority samples both fail closed despite a warm cached-true
observation. Crate tests are source proof; the installed-app prompt observation
(≤6 distinct, no repeats over several ticks) remains the acceptance gate.

## Completion still required

- Restored governance env handoff; Outbox/Watch tabs live again in the
  installed app.
- Grok Build seat available without version fingerprinting.
- Bounded Keychain authorization behavior on a fresh installed build (no more
  than the six distinct first-launch items: Gateway client key, auth pepper,
  Watch admin token, arm-principal token, Claude proxy token, Codex proxy
  token). **Source-complete** (see Auth-observation cache above); closing the
  checkbox still requires the installed-app observation.
- One governed Claude request and one governed Codex request from the installed
  app (explicit operator-approved acceptance; fail-closed proof that an
  unavailable proxy route never silently downgrades to Direct).
- Tauri, War Room, Gateway Pack, native/packaged smoke, DMG verification, and
  one final `make ship-check` on the exact final tree.
- Review, commit, push, and PR publication as separate operator-controlled
  seams.

## Stop conditions

Stop and report rather than expanding scope if:

- a proposed replacement adds more lifecycle code than it deletes;
- deterministic or packaged proof reveals loss of Direct CLI, War Room,
  Gateway Pack, or Tailscale behavior;
- completing a step would require a live provider call without explicit
  operator approval.
