# IRIN Codebase Simplification Plan

Status: execution plan

Validated against: `origin/main` at `acc6237f3f6e5449990759916d704b74b2fa7f59`
on 2026-07-29. PR 2 sequencing was corrected after the first implementation
attempt showed that making governed CLI migration a deletion prerequisite
created a second proxy lifecycle owner and coupled ownership cleanup to
Keychain, Docker, codesigning, TCC, and live-provider acceptance.

## Purpose

Simplify IRIN around the product that now exists. This is a source-reduction and architecture-convergence plan, not an installation guide.

There is no legacy-customer compatibility requirement. Nothing is protected merely because it was once a supported path. Conversely, an internal development or test tool is not dead merely because it is not a product surface; it stays only when it is the simplest reliable way to build or prove the product.

## Product truth

IRIN has one War Room UI source and two delivery contexts:

```text
council-rs/warroom/web
    |
    +-- foreground source development: make warroom
    |
    +-- static export bundled into the macOS DMG
            |
            +-- rendered in the Tauri webview
            +-- served by bundled Council on loopback
                    |
                    +-- published privately by DMG Settings through Tailscale Serve
```

The DMG is not a replacement for Tauri. It packages the Tauri host as `IRIN.app`. Tailscale is not a separate client surface: the installed app controls publication of the same War Room web export through Council. The standalone iOS wrapper is therefore duplicate delivery code, not the owner of phone access.

The simplification rule is: keep one implementation for each required behavior, keep internal tools only when they prove that implementation, and delete parallel operator/runtime/install paths.

## Validation of the Fable conclusion

The ending of `pooch.md` is directionally correct but stops too early.

### Confirmed

- There is one Tauri desktop application. `packaging/build-dmg.sh` builds and packages it as `IRIN.app` plus the DMG.
- `make app-install` is a duplicate source-built installation factory, not a third platform.
- `make app-install` is still live at this revision: a root target, a 143-line installer, a 240-line test, and references across product/operator documentation.
- `warroom-build` is called by the installer, its test, the native macOS smoke, and the root `warroom-tauri-build` wrapper.
- The DMG build stages the Gateway Pack; `warroom-build` does not.
- The ignored generated pack in this working copy currently differs from `gateway/lua` in all five top-level Lua files. That drift is local build state, not a tracked-HEAD fact. The writer exists (`scripts/stage-gateway-pack.sh`); the structural defect is incomplete use of that writer, not manual dual ownership.
- `_extract_sse_usage` in `gateway/lua/cost.lua` has no caller and is confirmed dead.
- Deleting the duplicate installer before changing bundle staging is the correct order.

### Corrections

- Deleting `app-install` is an intentional behavior removal, not “no behavior risk.” It changes a documented operator path and must remove its docs, tests, and CI/release-tree assumptions together.
- The surviving non-DMG build should not be upgraded into a second shippable app factory. The DMG pipeline should be the only production app builder; native development and smoke builds must be explicitly internal and non-promotable.
- Adding a third Gateway Pack mode merely to preserve the old standalone app builder would replace one fork with another. Prefer consuming the production build primitive or using an unmistakably test-only resource set.
- The standalone iOS shell is not needed when phone access is the War Room web surface over Tailscale. Its tracked tree is about 3,200 lines and is referenced outside itself only by release-tree allowlisting. Its remaining unique behavior—native Keychain storage and certificate pinning—must be replaced by a tested remote-browser authentication path before deletion.
- Fable omitted the source-managed runtime (`make setup`, `make runtime-*`, `scripts/irin-runtime.sh`). It duplicates lifecycle ownership already present in the DMG and accounts for about 1,999 lines across controller/setup tests before documentation and Tauri adoption logic are counted. Its unique behaviors are source-checkout launchd recovery, a managed Next.js listener, source Gateway provisioning, and Claude/Codex CLI proxy adapters. The first three can be retired; governed Claude/Codex CLI routing is product behavior that must migrate into the DMG-owned runtime before the source controller is deleted.
- Fable also omitted the DMG’s external-Council adoption and configurable source-path machinery. That code exists to coexist with the source-managed runtime. Removing only the shell scripts would leave the architectural fork embedded in Tauri and the web Settings model. Adoption can also report an exact-build Council as ready even when that source process lacks the packaged `--web-dist`, leaving the Tailscale root without War Room.
- Boot, deliberation, dispatcher, and ledger complexity is not automatically noise. It is live core code. It should be re-measured after surface deletion and then simplified according to measured complexity, churn, and authority risk.

## Target architecture

### Product entrypoints

- `make warroom`: foreground Council plus Next.js for source development of the web surface.
- `make dmg-build` and the release transaction: the only path that creates a promotable `IRIN.app`/DMG.
- Installed `IRIN.app`: owns its bundled Council, optional Gateway Pack, War Room static export, and the Tailscale Serve control.

### Internal entrypoints

- A native Tauri development command may remain inside `council-rs` because it shortens the edit/debug loop. It is not advertised as another way to install IRIN.
- A native smoke builder may remain only as a test harness. Its artifact must be non-promotable by construction and must not depend on stale gitignored resources.
- Gateway, Council, Sentinel, web, and packaging tests remain independently callable. Test topology is not product topology.

### Runtime ownership

- The installed app starts and owns the bundled Council. It does not adopt a separately managed same-build Council.
- An occupied Council port is a clear startup conflict, not a second supported ownership mode.
- Source development uses the foreground `make warroom` process tree. It is separate from the installed app and does not install login recovery.
- Tailscale Serve remains app-owned and port-scoped. It publishes the packaged Council’s War Room origin and optional Gateway routes; Funnel remains forbidden.

## Delivery sequence

Execute the work as sequential product PRs. Do not keep multiple overlapping cleanup branches alive.

### PR 1 — Remove duplicate delivery and install surfaces

1. Add a remote-browser regression that loads the Tailscale-style same-origin export, authenticates through the browser configuration path, and proves REST plus WebSocket access without the iOS wrapper.
2. Change `PhoneAccessControl.tsx` and related documentation from “War Room iPhone app/Keychain” instructions to the tested browser flow.
3. Delete `council-rs/warroom-ios/**`.
4. Remove the iOS file inventory and special allowlist from `scripts/check-release-tree.sh`.
5. Delete the root `app-install` target, `scripts/install-macos-app.sh`, and `scripts/test-install-macos-app.sh`.
6. Remove operator and product documentation that presents source-built app installation as supported.
7. Remove the root `warroom-tauri-build` alias and public documentation for `warroom-build` as an artifact-producing command.
8. Remove the root `warroom-tauri` alias from the operator-facing target list. Retain the lower-level native dev launcher only if the Tauri development loop still uses it.
9. Update CI path-classifier fixtures and workflow assertions made obsolete by these deletions.

Each commit should delete one complete reachable slice: implementation, tests, documentation, and release-tree/CI references together.

Completion evidence:

- No tracked `warroom-ios`, `app-install`, or `install-macos-app` reference remains.
- Root help presents the web development and DMG product paths without another installation or desktop-package entrypoint.
- `make release-check`, the CI classifier self-test, web checks, Tauri Rust tests, and the native smoke remain green.

### PR 2 — Collapse runtime ownership into the DMG

This PR is ownership-only. It must remove the competing Council, web, login,
and adoption lifecycle without adding a second implementation of governed
Claude/Codex proxy lifecycle.

1. Retire or rewrite the native smoke that starts and adopts an external
   Council. Prefer the packaged full-app smoke, which already proves app-owned
   spawn, shutdown, port-conflict isolation, and the embedded web surface; keep
   a smaller native smoke only for behavior the packaged smoke does not cover.
2. Make packaged Tauri startup own the bundled Council unconditionally.
3. Remove matching-build adoption, external-runtime restart messaging, and the
   branch that treats an unpackaged release shell as a supported runtime.
4. Remove `councilPath` and `councilRoot` from the user-facing Settings/config
   contract and Tauri command arguments. Keep fixed test injection seams only
   where automated tests require them.
5. Remove source-checkout Council path resolution and base-directory override
   logic that becomes unreachable. Packaged writable state remains under
   Application Support; source web development continues to use the checkout.
6. Retire source-checkout login recovery and the managed Next.js listener in
   favor of foreground `make warroom`.
7. Remove the source controller’s Council, Next.js, login LaunchAgent,
   MatchingBuild, and Settings ownership surfaces, along with their dedicated
   tests, Makefile targets, and operator documentation.
8. Move any still-required developer-only Gateway configuration helper under
   the Gateway development surface. Do not retain an entire product runtime
   controller merely to generate test configuration.
9. Preserve the existing governed Claude/Codex route, if still required during
   the transition, only as an explicitly temporary and optional proxy launcher.
   The shim may start and stop the existing CLI proxies and nothing else.
10. Simplify restart and stop ownership around the one app-owned Council child.

The optional governed-CLI shim is not a second runtime owner. It must not spawn
Council or Next.js, install a login LaunchAgent, adopt a MatchingBuild process,
or expose `councilPath`/`councilRoot`. If extracting it requires retaining a
substantial fraction of `scripts/irin-runtime.sh`, the extraction has failed
and must stop for redesign.

Do not port the Python proxy implementation into Tauri in this PR. In
particular, do not add a native adapter server, proxy-token Keychain migration,
adapter health/restart subsystem, or live Claude/Codex acceptance gate.

This PR should materially reduce `try_start_council_server`, `restart_sidecar`,
`sidecar.rs`, `paths.rs`, runtime-config fields, Settings components, and their
tests—not merely delete shell wrappers or replace them with Rust.

Completion evidence:

- No product code or documentation instructs the operator to reconcile an installed app with `make setup` or `make runtime-*`.
- The DMG cold-starts its bundled Council without Rust, Node, a checkout, or a separately installed runtime.
- Closing the app terminates only processes it owns.
- Foreground `make warroom` remains usable for browser development.
- Tailscale publication still serves the packaged War Room origin and never mutates unrelated Serve ports or Funnel.
- Any retained governed-CLI shim owns only the two existing proxy processes and
  is absent from installed-app Council lifecycle, source login recovery, and
  product Settings.
- Tauri Rust tests, War Room web/export tests, native or packaged smoke, Gateway
  Pack tests, and a fresh local DMG build pass without a live provider call.

### PR 2a — Decide and migrate governed CLI transport

Treat governed Claude/Codex routing as a separate product migration with its
own architecture decision and acceptance surface.

1. Confirm whether the Docker-to-host HTTP proxy remains necessary or whether a
   governed host transport can invoke the existing CLI path without the proxy
   hop.
2. Account explicitly for macOS responsible-process identity, Keychain access,
   codesigning, and TCC behavior when an installed app launches provider CLIs.
3. Prefer deleting synthetic proxy tokens and duplicate lifecycle over porting
   them. Do not weaken authentication merely to avoid Keychain prompts.
4. Preserve fail-closed governed routing and independently selectable Direct
   mode. Never silently downgrade governed traffic to Direct.
5. Add deterministic route, lifecycle, and failure tests before any live call.
6. Perform one operator-approved installed-app acceptance with exactly one
   governed Claude request and one governed Codex request. Stop on unexpected
   Keychain repetition, TCC expansion, retry, or adapter instability.
7. Delete the temporary source proxy shim only after the replacement passes
   deterministic and live acceptance gates.

Completion evidence:

- One implementation owns governed CLI transport.
- No duplicate Python/Rust proxy lifecycle remains.
- Secret material is neither logged nor persisted outside its intended
  operator-owned store.
- Installed acceptance requires only the documented bounded authorization
  sequence and requests no unrelated Documents, Music, or broad filesystem
  access.
- Exactly one governed Claude and one governed Codex request pass without
  retry, downgrade, or adapter loss.

### PR 3 — Make the DMG the sole production app factory

1. Extract one internal app-bundle build primitive from `packaging/build-dmg.sh` only if both the DMG build and native smoke genuinely need to invoke the same compilation steps.
2. Keep production staging in one ordered pipeline: Council binary/base resources, War Room static export, Gateway Pack, Tauri build, signing, DMG creation, receipts.
3. Remove `council-rs` `warroom-build` and `warroom-sign` once no caller treats them as a general artifact factory.
4. Make any remaining native smoke consume the app created by the shared build primitive or build into a temporary target with the existing unique smoke identifier and a deterministically staged inert Gateway resource fixture. It must never write a smoke app into the canonical production bundle output.
5. Extend `scripts/test-gateway-pack-assets.sh` with a deterministic freshness assertion: after staging to a temporary destination, the staged Lua tree must be content-identical to `gateway/lua`.
6. Keep the existing DMG receipt hashes and verifier checks as artifact proof. Do not make a gitignored staging directory an authority source.
7. Fold `scripts/build-warroom-web-tauri.sh` into the canonical War Room asset builder so one static-export path feeds Tauri, Council `--web-dist`, export tests, and packaging without repeated npm wrapper layers.
8. Remove unused public build/export/smoke targets, npm aliases, and old `council-rs/warroom/scripts` launchers after a zero-caller check.
9. Delete `_extract_sse_usage` from the tracked Lua source; the staged copy follows from the writer.
10. Audit standalone manual tooling such as `council_audit.py`, `sheldon_eval.py`, and raw Council-binary release upload. Delete each when it has no product or CI caller; do not preserve it as historical inventory.
11. Update build and CI names so “Tauri” identifies the implementation layer and “DMG” identifies the shippable artifact, without presenting them as competing products.

Completion evidence:

- A repository search finds one production `tauri build` orchestration path.
- No command outside the packaging/release path produces a promotable `IRIN.app`.
- A clean checkout cannot silently package stale Gateway Lua.
- Native smoke and the DMG use the same embedded War Room export contract.
- `make dmg-build`, `make dmg-verify`, applicable CI lanes, and `make ship-check` pass on the exact tree.

### PR 4 and later — Simplify the core that remains

Re-index and re-measure after PRs 1–3. Deleting runtime modes and configuration branches changes the graph, so the current ranking must not be treated as permanent.

Current baselines worth rechecking:

| Function | Current score | Plan |
| --- | ---: | --- |
| `engine/deliberate.rs::run_with_cancel` | 233 | Core Council behavior; extract named phases only after the surface/runtime cleanup lands. |
| `gateway/sidecar-rs/src/boot.rs::load_config_build_state_and_serve` | 228 | Required Gateway boot; split configuration, authority initialization, state hydration, listener startup, and shutdown into tested phases. |
| `gateway_ledger.rs::cmd_fsck` | 182 | Separate parsing/reporting from ledger traversal if it remains high after rebaseline. |
| `main.rs::run_deliberation_cli` | 180 | Remove CLI-only branching that is no longer reachable, then extract command phases. |
| Tauri `lib.rs::try_start_council_server` | 122 | Expected to fall substantially in PR 2; do not add a second refactor before measuring the deletion result. |
| Watch dispatcher top function | 100 | Authority-sensitive and low-churn; refactor only if the post-cleanup graph still ranks it above actively changing code. |

For every structural extraction:

1. Capture the pre-change score and existing behavior tests.
2. Move one domain-named phase without changing its inputs, outputs, ordering, or authority checks.
3. Run the crate-local tests and the product proof appropriate to the path.
4. Recompute the parent score and dependency cycles.
5. Keep the change only when complexity falls measurably without adding a new high-severity cycle or widening an interface.

This phase is not a generic “split large files” campaign. Size alone does not justify a refactor, and stable authority code should not be churned for aesthetics.

## Verification matrix

| Changed area | Required proof |
| --- | --- |
| Surface/entrypoint deletion | reference search, release-tree check, CI classifier self-test |
| War Room web or static export | lint, typecheck, frontend tests, embedded export test |
| Tauri runtime ownership | Tauri Rust tests, native macOS smoke, cold-launch behavior |
| Gateway Pack staging | asset/isolation tests, staged-content equality, packaged smoke where applicable |
| Governed Claude/Codex CLI migration | deterministic proxy/Gateway tests, fail-closed routing proof, then explicit operator-approved installed-app acceptance |
| Tailscale publication | pure route/ownership tests plus bounded installed-app behavior; no Funnel or global reset |
| Gateway boot/watch/signing | crate tests plus `make verify` |
| Council deliberation | crate tests, web socket/stream tests, no-spend behavior proof |
| Final product tree | `make ship-check`, fresh DMG build/verify, installed cold launch, private web access over Tailscale |

A green source test, a built DMG, an installed cold launch, and working Tailscale publication are separate facts. The cleanup is complete only when the exact final tree clears all applicable facts.

## Expected reduction

The first two PRs remove more than 5,500 tracked lines from the standalone iOS shell, duplicate installer, and source-runtime controller/tests before counting documentation, release-tree allowlists, Tauri external-adoption logic, or frontend configuration branches. The more important result is architectural: one UI source, one installed runtime owner, one production app factory, and one private web publication path.
