# CI operating model

IRIN uses a stable CI aggregate, `ci / CI required`, backed by scoped proof
lanes. Branch protection pairs it with stable CodeQL and dependency-review
checks. The model is local-first: trusted project work can use
persistent self-hosted capacity, while code outside the trust predicate runs
only on GitHub-hosted runners.

## Event contracts

| Event | Runner | Proof scope |
| --- | --- | --- |
| Private-repository, same-repository pull request authored by `iws17` | Restricted runner group with the `irin-ci` label; selected Tauri shell work uses `macos-15` | Changed-path lanes plus always-on checks |
| Public repository, fork, bot, or any other pull request | `ubuntu-latest`; selected Tauri shell work uses `macos-15` | Changed-path lanes plus always-on checks |
| Merge-queue check (`merge_group`) | Restricted runner group while private; `ubuntu-latest` when public; Tauri shell work uses `macos-15` | Path-scoped to the merge group's exact `base_sha...head_sha` diff plus always-on checks |
| Push to `main` | `ubuntu-latest`; Tauri shell work uses `macos-15` | Path-scoped to that push's exact `before...sha` diff plus always-on checks; not a full integrated matrix |
| `workflow_dispatch` | Restricted runner group while private; `ubuntu-latest` when public; Tauri shell work uses `macos-15` | Full matrix; Gateway smoke remains explicit opt-in; an opted-in private `main` run also performs the exact-source no-spend proof |
| Nightly schedule | Restricted runner group while private; `ubuntu-latest` when public; Tauri shell work uses `macos-15` | Full matrix |
| Release tag | `ubuntu-latest` in `release.yml` | Release build and release checks |

### Pull-request entry and the `@main` trust boundary

Pull requests enter through the thin `ci-pr.yml` dispatcher. Its stable `ci`
job calls `irinityhq/irin/.github/workflows/ci.yml@main` — the reviewed base
copy on `main`, not the PR-head checkout. That pin keeps runner-selection and
self-hosted trust predicates under base control so a one-commit PR cannot
rewrite `runs-on` and escape to a hostile runner. GitHub preserves the pull
request event context for the called graph, so scope classification and
label-gated lanes retain their normal behavior. Called job names receive the
`ci /` prefix; keeping the caller job id stable preserves the required
`ci / CI required` context.

Merge-queue checks enter through the same dispatcher and the same base pin, so
the protected context name is identical for a queued merge and an ordinary pull
request. `ci.yml` intentionally has no top-level `merge_group` trigger: that
would dual-run a second graph whose job names lack the `ci /` prefix and never
satisfy the protected context.

**Consequence:** an ordinary PR that edits `ci.yml` is linted and classified,
but the proposed workflow revision is **not** executed by the ordinary PR
graph. For CI-workflow changes, run a hosted `workflow_dispatch` on the branch
tip and retain that receipt as the explicit same-revision validation. The
branch dispatch is an additional CI-workflow-change receipt, not a replacement
required context and not a substitute for PR-event-only predicates (those are
covered by deterministic local/contract tests).

Do not switch the dispatcher to `uses: ./.github/workflows/ci.yml` without a
design that proves same-SHA execution and base-controlled runner trust at the
same time.

### Main push: path-scoped proof and bounded queue

Ordinary pushes to `main` classify from the event's exact `before...github.sha`
diff. Lightweight merges therefore run only the selected lanes; packaging-
sensitive merges still pay macOS candidate/install work when the base-
controlled exact-path policy selects them. A zero or unavailable `before` fails
safe to the full non-SBOM matrix.

**Not every `main` SHA produces a durable candidate.** Exact candidate and
install artifacts remain path-selected. Do not infer a candidate, shipping
tier, install acceptance, publication, or deployment from a green lightweight
main run.

Main pushes share one non-cancelling concurrency group with `queue: max` so
merge receipts are retained up to GitHub's pending cap of **100** runs in that
group. Additional runs beyond the cap are cancelled (not a lossless infinite
queue). GitHub's default concurrency queue keeps only one pending run; without
`queue: max`, a smaller burst can already replace an earlier pending receipt
while the next path-scoped run examines only its own `before...sha` and never
covers the discarded merge. Operator recovery if the cap is approached or
overflow cancels a run: stop the held-fix merge cadence, let the queue drain,
re-run the cancelled main SHA (or re-push an empty commit only when necessary),
and do not treat a later path-scoped run as covering a missing receipt.
Superseded-PR cancellation lives only on `ci-pr.yml`, which keys its group on
the pull-request number with `cancel-in-progress: true`. On that same
dispatcher, merge-queue runs key on the merge group's head SHA and set
`cancel-in-progress: false`, so a queued merge cannot be superseded by
ordinary pull-request updates. Neither group shares a concurrency mapping with
`queue: max`.

Scheduled and manual runs use the full integrated matrix and unique concurrency
groups; they do not join the main-push queue. Full-matrix proof is also
base-forced for pull requests that change workflows, actions, the path
classifier, or classifier/control-plane contract tests — computed from the
base-owned path list, not from PR-produced classifier booleans.

While the repository is public, runs use GitHub-hosted runners. Private-only
runner predicates and the private-only exact-source Gateway smoke remain in the
workflows for a future private posture but are dormant while public; do not
delete them as "unused."

## Required and scoped checks

Every pull request reports these jobs, including documentation-only changes:

- GitHub Actions lint;
- path classification;
- root secret scan;
- security scanners (advisory Opengrep + Selene; findings non-blocking);
- public-tree hygiene;
- public PR language; and
- the `ci / CI required` aggregate.

The aggregate accepts successful or intentionally skipped scoped lanes and
fails if any selected lane fails. Actionlint is installed from a pinned,
checksum-verified release and validates every workflow before the aggregate can
pass. The detection job first runs the classifier's
self-test and validates its complete Boolean output schema, so broken or missing
classification output fails closed rather than skipping every heavy lane. It is
the stable aggregate for the scoped CI lanes; security workflows remain separate
required branch-protection contexts.

`scripts/classify-ci-paths.sh` is the path-to-lane contract. Its table-driven
self-test covers documentation, component runtime source, War Room web, Tauri,
workspace manifests, the shared protocol crate, CI definitions, full-proof
events, and unknown paths. Unknown paths select the full matrix rather than
silently missing proof.

Exact candidate and exact install gates additionally use a **base-controlled
inline overlay** in `ci.yml` so a PR cannot rewrite the classifier to skip
isolation. A separate base-controlled force-full guard raises the full non-SBOM
PR matrix when CI policy surfaces change. `scripts/test-ci-control-plane.sh`
binds the inline exact-path policy to the classifier and proves a hostile
all-false classifier cannot suppress force-full lanes.

The heavy lanes are intentionally separate:

- Gateway, Council, and Sentinel Rust source select their applicable Rust
  checks. Cargo manifests, lockfiles, and the shared deny policy select the
  applicable supply-chain checks; ordinary source edits do not rerun them.
- War Room web source and npm lockfiles select a Linux-safe web gate: lint,
  typecheck, unit tests, hosted browser tests, embedded-export browser tests,
  the exact static export, and a production npm advisory gate. They also select
  the macOS Tauri lane because the desktop shell embeds that export. They do not
  select Council Rust or Cargo SBOM work.
- Tauri source selects the native-shell smoke on `macos-15`. Its manifest or
  lockfile selects the standalone Tauri Cargo audit and deny checks; the shared
  deny policy also selects that supply-chain lane. The Linux Web lane never
  compiles the unsupported native Linux Tauri shell. The macOS lane owns Tauri
  Rust tests, native bundle, and process evidence. The required local macOS ship
  gate aggregates both suites and owns native visual/window evidence because
  hosted CI records only a headless process proof.
- SBOMs are generated by scheduled and manual full proofs, not by ordinary pull
  requests.

Gateway Compose smoke remains label-gated on pull requests and input-gated on
manual runs. Its Watch producer is explicitly disabled, it uses test-only
credentials, and cleanup runs even after failure. On a private-repository
manual dispatch from `main` with `run_gateway_smoke=true`, the same self-hosted
job then runs the canonical `make verify` no-spend proof from the checked-out
source. That proof builds with `DEMO_ALLOW_BUILD=1`, forbids published-image
pulls with `DEMO_PULL=0`, and isolates its Compose project and image names by
commit SHA, run ID, and run attempt. Its stack and run-specific images are
cleaned even if the smoke or exact-source proof fails. This extra proof is
intentionally excluded from pull requests and public-repository dispatches.
Pull-request workflows do not call live model providers or consume repository
secrets.

## Persistent-runner state

Rust setup gives each self-hosted runner process persistent, isolated
`CARGO_HOME` and `RUSTUP_HOME` directories. This prevents concurrent runner
processes on one host from racing while installing or updating a toolchain. A
content-addressed `SCCACHE_DIR` and a deterministic port in the range
`42001`–`42006` are also stable and isolated for the supported
`r740-runner-1` through `r740-runner-6` names. An unexpected runner name does
not receive `RUSTC_WRAPPER`, avoiding an ambiguous shared compiler server.
This preserves reuse across a supported runner's jobs without allowing one
job's process cleanup to interrupt another runner's compiler service.
Each self-hosted job explicitly starts and probes its runner-isolated `sccache`
server before Rust work begins; the cache directory persists even when runner
cleanup stops the prior job's server process. Self-hosted jobs use an installed
`sccache` only when it is available and do not upload or restore GitHub Actions
caches. Hosted jobs retain Actions caching.

Persistent runners are provisioned with `rg`, `sccache`, Chromium's system
dependencies, and the other host packages used by selected lanes. Required
jobs verify their self-hosted prerequisite before use instead of silently
installing mutable host packages during every run.

Web jobs always install the Chromium browser. Hosted jobs also install its
system dependencies; persistent self-hosted jobs rely on provisioned system
packages instead of reinstalling them for each run.

## CodeQL advanced setup

`CodeQL Advanced` runs on pull requests, merge-queue checks, pushes to `main`,
a weekly schedule, and manual dispatch. It uses the restricted runner group
only for the private, same-repository trusted path and GitHub-hosted capacity
otherwise. It analyzes
Actions, JavaScript/TypeScript, Python, and Rust independently, then reports
the stable `CodeQL required` aggregate. The aggregate is a separate
branch-protection context rather than part of `ci / CI required`.

Dependency review runs on pull requests and merge-queue checks with read-only
permissions and a full-SHA-pinned GitHub action. Its stable `Dependency Review`
check rejects newly introduced vulnerabilities of moderate severity or higher.
Merge-queue runs pass the merge group's base and head refs explicitly, because
the action derives them automatically only on a pull-request event.

## Review settlement (pre-queue)

Solo-maintainer branch protection keeps `required_approving_review_count` at
zero and enables conversation resolution. That combination does **not** wait
for a pending review *request* to clear: a requested bot or human review can
still be in flight while a PR enters the merge queue. PR #70 is the reference
incident (Copilot requested, queue/merge completed, review landed afterward on
the same head with actionable threads).

`Review settlement` is a stable required-check *producer* in
`.github/workflows/review-settlement.yml`. It reuses the existing required
status-check and merge-queue mechanism (not a second control plane). The job
name is the protected context name. Evaluation lives in
`scripts/check-review-settlement.sh` and is covered by deterministic fixtures
in `scripts/test-ci-control-plane.sh`.

Fail closed when any of the following hold on the current pull-request head:

- `reviewRequests` is nonempty;
- a non-dismissed latest review is not bound to the current `headRefOid`
  (a new commit invalidates prior settlement);
- `CHANGES_REQUESTED` is present on the current head; or
- an actionable review thread remains unresolved (open and not outdated).

Event surfaces: pull-request open/reopen/synchronize/ready/draft conversion,
review requested/removed, pull-request review submitted/edited/dismissed, and
`merge_group` `checks_requested`. Draft PRs are treated as not-ready (the check
passes with a note); `ready_for_review` re-evaluates.

**GitHub boundary (explicit):** there is no reliable dedicated workflow event
for “thread resolved.” Conversation-resolution branch protection remains the
complementary layer for that transition; this check re-reads thread state
whenever a review or head-SHA event fires and again on the merge-queue check.
Register `Review settlement` as a required context only after the job has
appeared on a real pull request. Do not rename the job lightly.

## Branch-protection contract

Once all contexts have appeared on a real pull request, `main` requires:

- `ci / CI required`;
- `CodeQL required`;
- `Dependency Review`;
- `Review settlement` (after first real-PR registration; manual follow-up).

The exact names are operating contracts. Rename a caller job or aggregate only
in a maintenance window where the replacement context first registers on a
real pull request. Main protection also applies to administrators.
`CODEOWNERS` records the maintainer for authority-bearing paths, but does not
require a second personal account to approve the primary maintainer's changes.
The required CI, CodeQL, dependency-review, review-settlement, current-base, and
conversation-resolution gates provide the solo-maintainer merge boundary.

## Promotion and pre-public checklist

CI architecture changes are developed as stacked commits on one branch and
promoted through one pull request after the full stack is locally verified and
explicitly approved.

For the public repository:

- confirm every workflow routes to GitHub-hosted runners when the repository is
  public, while private trusted work still routes to the restricted group;
- confirm the self-hosted runner group does not allow public repositories;
- keep the trusted author and same-repository predicate enforced;
- revisit runner ephemerality when untrusted code can enter the trusted path;
- confirm main pushes are path-scoped and that the main concurrency queue uses
  `queue: max` with no cancel on that group (bounded retention: 100 pending;
  overflow cancelled; serial held-fix cadence);
- for CI-workflow changes, retain a branch `workflow_dispatch` same-revision
  receipt in addition to ordinary PR CI against `ci.yml@main`;
- register required check names before changing branch protection;
- use a deliberately failing pull request to prove every required context
  blocks merge;
- retain one branch, stacked commits, and one promotion pull request; and
- keep release attestation, signed-commit enforcement, and a narrower Actions
  allowlist as separately reviewed hardening rather than overstating them here.
