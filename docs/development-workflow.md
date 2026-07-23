# Development workflow

IRIN treats a change as ready only when both the requested behavior and the
affected product surfaces are proven. This matters most for the War Room: its
Web source is also embedded in the native Tauri application, so a browser-only
result cannot establish that the desktop product still works.

## One lifecycle

```text
make worktree → make preflight → edit and make check → make ship-check → pull request
```

Use one logical change, named branch, owner, and linked worktree. Runtime ports,
Compose names, state directories, generated Web assets, and native application
processes must remain scoped to that worktree. A launcher must refuse an
occupied port rather than terminate a process it does not own.

Create a worktree from the canonical checkout:

```bash
make worktree BRANCH=fix/example
cd ../irin-wt-fix-example
```

The creator fetches `origin/main`, creates the branch from that exact commit,
writes a collision-checked ignored worktree runtime profile, requires and
registers the linked worktree with Gortex, and runs the initial preflight. If setup fails, it
removes the incomplete worktree rather than leaving a half-configured checkout.

Remove a finished clean worktree while retaining its branch:

```bash
make worktree-remove DEST=/absolute/path/to/worktree
```

The removal gate refuses main, detached, or dirty worktrees, stops the isolated
runtime, removes the Git worktree, and unregisters it from Gortex.

## Gortex is MCP-first

The operator keeps one Gortex daemon and MCP service running. Each agent should
use the MCP in this order:

1. `smart_context` at task entry;
2. `get_editing_context` before a non-trivial source edit;
3. `detect_changes` and affected-test analysis before completion;
4. `change_contract` when a shared protocol, signing, outbox, communications,
   capability, or CI authority surface is touched.

`make preflight` verifies the daemon, configured clients, exact worktree
registration, and index freshness. A daemon that is merely running is not
enough: the indexed path and commit must belong to the current linked worktree.

If the MCP is configured but not visible to the current client, state
`GORTEX_MCP_MISSING` in the work report and use the named continuity path:

```bash
scripts/gortex-worktree.sh detect
```

That command invokes the daemon's `detect_changes` tool through `gortex call`.
It keeps work moving while preserving the fact that client discovery needs
repair. Do not substitute an old main-checkout index for the linked worktree.
Managed operator worktrees and `make ship-check` require the Gortex CLI
continuity path to succeed. `make check` remains usable in an ordinary public
checkout without private operator tooling, and CI does not depend on Gortex.

## The three gates

### `make preflight`

Run before editing. It rejects main, detached HEAD, a dirty starting tree, an
untracked or stale Gortex worktree when Gortex is installed, and missing Git
base information. It records the current `origin/main` commit and prints the
worktree's Council, Web, and Gateway ports.

### `make check`

Run during implementation. The existing CI path classifier selects focused
Rust, Web, embedded-export, or Tauri tests. It also records a Gortex
`detect_changes` result when Gortex is available. This is the fast feedback
loop, not the shipping claim.

### `make ship-check`

Run immediately before claiming completion or updating the pull request. It:

- refuses a receipt based on an older `origin/main`;
- reruns the Gortex change and impact pass;
- runs the full local equivalents for every selected CI lane;
- treats every War Room Web change as a Tauri product change;
- proves hosted Next behavior, the exact embedded static export, Tauri Rust,
  and a native macOS application launch and visible-surface smoke;
- rejects high or critical production npm advisories;
- runs release-tree, public-language, secret, and whitespace checks; and
- writes an ignored receipt under `.irin-receipts/` with the branch, commits,
  complete changed-file set, deterministic tested-tree fingerprint, lanes,
  commands, results, and completion time.

If pinned tooling is absent, the gate downloads `cargo-deny` 0.19.9 and
actionlint 1.7.12 into the ignored `.irin-tools/` directory and verifies each
published SHA-256 before execution. Actionlint validates every GitHub Actions
workflow as part of the ship receipt. `make tools` performs both bootstraps
explicitly.

No current passing receipt means no `done`, `ready`, or `safe to merge` claim.
If another pull request merges first, update from `origin/main`, rerun
`make preflight`, then rerun the ship check. The integrated `main` workflow
repeats the complete code matrix after merge so individually green branches
cannot produce an untested combined tree. Scheduled and manual proof continue
to own SBOM generation.

## Product regression boundary

The War Room gate has three distinct proofs:

1. Hosted Playwright tests exercise the browser-served Next application.
2. Export Playwright tests serve `warroom-web-dist`, the exact assets embedded
   by Tauri, and repeat the full hosted Playwright corpus against that export.
3. The required local macOS ship smoke builds and launches the native application, proves its
   process and window remain alive, captures only that application window, and
   verifies visible core navigation text. It uses no provider credentials and
   does not arm Watch or execute a real action. CI separately records a
   headless process proof and labels it as such; it does not claim visual proof.

Artifact marker searches remain quick diagnostics, not product evidence.
Provider calls, paid deliberation, Watch arming, and external mutations remain
outside all routine gates.
