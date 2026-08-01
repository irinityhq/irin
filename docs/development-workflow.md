# Development workflow

IRIN treats a change as ready only when both the requested behavior and the
affected product surfaces are proven. This matters most for the War Room: its
Web source is also embedded in the desktop application (Tauri is the
implementation; `make dmg-build` is the sole production IRIN.app factory), so
a browser-only result cannot establish that the desktop product still works.

## One lifecycle

```text
make worktree → make preflight → edit and make check → make ship-check → push once → pull request
```

Use one logical change, named branch, owner, and linked worktree. Prefer one
subsystem per pull request (`council-rs`, `gateway`, `sentinel`, packaging, or
CI), stacked commits inside that PR, and blast radius over file-count religion.
Iterate locally until `make check` is green, run `make ship-check` once before
the PR is marked ready, then push once; do not use the remote PR as a review
scratch pad. Runtime ports,
Compose names, state directories, generated Web assets, and native application
processes must remain scoped to that worktree. A launcher must refuse an
occupied port rather than terminate a process it does not own.

Create a worktree from the canonical checkout:

```bash
make worktree BRANCH=fix/example
cd ../irin-wt-fix-example
```

The creator fetches `origin/main`, creates the branch from that exact commit,
writes a collision-checked ignored worktree runtime profile, and runs the
initial preflight. If setup fails, it removes the incomplete worktree rather
than leaving a half-configured checkout.

Remove a finished clean worktree while retaining its branch:

```bash
make worktree-remove DEST=/absolute/path/to/worktree
```

List (or remove with `APPLY=1`) clean worktrees already merged into
`origin/main`:

```bash
make worktree-gc
make worktree-gc APPLY=1
```

The removal gate refuses main, detached, or dirty worktrees, stops the isolated
runtime, and removes the Git worktree. Managed worktrees also set a shared
`CARGO_TARGET_DIR` under `~/.cache/irin/cargo-target` so the next worktree
reuses compiled artifacts instead of a cold multi-GB build.

Every tracked `build.rs` is also required to have an explicit CODEOWNERS entry.
The public-tree gate fails when a new build-time execution surface is added
without authority review.

## The three gates

### `make preflight`

Run before editing. It rejects main, detached HEAD, a dirty starting tree, and
missing Git base information. It records the current `origin/main` commit and
prints the worktree's Council, Web, and Gateway ports.

### `make check`

Run during implementation. The existing CI path classifier selects focused
Rust, Web, embedded-export, or Tauri tests. This is the fast feedback loop,
not the shipping claim.

### `make ship-check`

Run immediately before claiming completion or updating the pull request. It:

- refuses a receipt based on an older `origin/main`;
- runs the local equivalents for every selected CI lane only (documentation
  and development scripts stay on always-on light checks; a single Rust
  crate uses package-scoped fmt/clippy/test; full matrix or multi-crate fan-out
  still runs the workspace);
- treats every War Room Web change as a Tauri product change;
- proves hosted Next behavior, the exact embedded static export, Tauri Rust,
  and a native macOS application launch and visible-surface smoke when those
  lanes are selected;
- rejects high or critical production npm advisories;
- runs public-tree, public-language, secret, and whitespace checks; and
- writes an ignored receipt under `.irin-receipts/` with the branch, commits,
  complete changed-file set, deterministic tested-tree fingerprint, lanes,
  commands, results, and completion time.

Prefer one ship-check per PR. Re-run only when `origin/main` moved or the
receipt failed. Keep open full-matrix pull requests to a minimum so branches do
not invalidate each other in a re-proof loop.

If pinned tooling is absent, the gate downloads `cargo-deny` 0.19.9 and
actionlint 1.7.12 into the ignored `.irin-tools/` directory. It verifies the
published archive SHA-256 and the platform-specific executable SHA-256 before
installation, then rechecks the cached executable on every use. Actionlint
validates every GitHub Actions workflow as part of the ship receipt. `make
tools` performs both bootstraps explicitly.

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
3. The required local macOS ship smoke builds and launches a non-promotable
   native test app (isolated bundle id; not the production DMG path), proves its
   process and window remain alive, captures only that application window, and
   verifies visible core navigation text. It uses no provider credentials and
   does not arm Watch or execute a real action. CI separately records a
   headless process proof and labels it as such; it does not claim visual proof.
   Ship the product artifact with `make dmg-build` (release transaction for
   production).

Artifact marker searches remain quick diagnostics, not product evidence.
Provider calls, paid deliberation, Watch arming, and external mutations remain
outside all routine gates.
