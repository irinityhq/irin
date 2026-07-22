# Contributing to IRIN

Thanks for looking at IRIN. This is local-first, single-operator software under
active development — contributions are welcome, and the bar is honesty over
volume: a good bug report or a one-line doc fix is worth more than a large
speculative change.

By contributing you agree your work is licensed under the repository's
[Apache License 2.0](LICENSE). There is **no CLA** to sign.

## Before you start

- Read the [README](README.md). On macOS, follow its `make setup` path; on
  Ubuntu, use `make warroom`. Then open Discover and Deliberate. `make verify`
  is an engineering regression for the separate signed-directive lane, not a
  product demo.
- IRIN is a mono-repo assembled from three subtrees (`gateway/`, `council-rs/`,
  `sentinel/`). Root-level files (CI, `Makefile`, workspace `Cargo.toml`,
  `.gitleaks.toml`) are owned here; most product code lives under a subtree with
  its own `README.md` and docs. Start in the subtree your change touches.

## Building and testing

Every unqualified `make ...` command in this file runs from the IRIN repository
root. A component-only target is always written as `make -C <component> ...`.

The verify lane needs only Docker plus common CLI tools on macOS or Ubuntu.
Building and testing the Rust workspace needs the Rust toolchain (and Node 20+
for the War Room UI) — see the prerequisite list under "Get started" in the
README.

Core workspace checks:

```bash
make build         # cargo build --workspace --release
make test          # cargo test --workspace
make release-check # release-tree completeness and hygiene
```

The separate integration proof exercises the disarmed, no-provider
Sentinel-to-signed-directive lane in an isolated Docker stack:

```bash
make verify
make verify-down
```

For frontend development on macOS or Ubuntu, follow
[`council-rs/docs/war-room.md`](council-rs/docs/war-room.md). Its standalone
`make warroom` launcher and the macOS `make setup` runtime both use the
default Council port, so do not start both independently.

Please make sure `make test` passes and the code is `cargo fmt`-clean before
opening a PR. CI always runs change classification, the root secret scan,
release-tree hygiene, and public-language checks. It selects Rust, supply-chain,
War Room web, and Tauri lanes from changed paths. Scheduled and manual full
proofs also generate SBOMs. See the [CI operating model](docs/ci-operating-model.md)
for the complete contract.

## Pull requests

1. **Open an issue first for anything non-trivial.** A bug report or a short
   proposal saves you from building something that collides with in-flight work
   or the deliberate non-goals below. Typo and doc fixes can skip straight to a
   PR.
2. **Fork and branch.** One logical change per PR. Keep the diff focused —
   unrelated cleanups belong in their own PR.
3. **Commit messages** follow `type(scope): summary` — e.g.
   `fix(warroom): …`, `docs(readme): …`, `feat(gateway): …`,
   `ci: …`, `chore: …`. Scope is optional; a clear imperative summary is not.
   Commit messages are public release notes: do not include agent session URLs,
   private host or org paths, local-only process notes, tool transcripts, or
   attribution footers from coding assistants.
4. **CI must stay portable.** Fork, bot, and other untrusted pull requests run
   on GitHub-hosted runners. The current operator's same-repository pull
   requests may use the restricted self-hosted runner group under the explicit
   repository-and-author predicate. Do not add a required dependency on an
   operator service outside the documented CI runner contract.

Fork contributors can run the build, test, lint, browser War Room, and isolated
verification targets on macOS or Ubuntu. The managed macOS product runtime
(`make setup`, `make runtime-up`)
intentionally accepts only the canonical `irinityhq/irin` origin so its source
receipt cannot adopt an arbitrary fork. Maintainers who need a live development
runtime use `make worktree BRANCH=...` from the canonical clone.

### Public PR language

PR titles and descriptions are part of the public project record. Write them
for a reader who wants to understand the shipped behavior, not the private
execution path used to produce it.

- Describe what changed, why it matters, and how it was verified.
- Leave out internal process narrative: agent seat names, model/session URLs,
  temporary routing notes, private org or host paths, and tool transcript
  excerpts.
- Avoid roadmap codenames or launch/ops shorthand unless the name already
  appears in public docs as the thing being changed.
- Name a provider, CLI, or tool only when it is part of the user-visible
  behavior or API surface in the diff.
- Keep bot-generated summaries and review prompts out of PR bodies; summarize
  the final human intent instead.

CI hard-fails a short list of high-confidence leak shapes in the **PR title,
PR body, and commit messages** (session trailers and session URLs, absolute
personal home paths, assistant attribution footers and co-author trailers).
It does **not** scan the repository tree — naming a supported tool in docs or
code is fine. Exact patterns and a local runner live in
`scripts/check-public-pr-language.sh` (`--self-test` / `--range BASE..HEAD`).

### Sensitive paths get a heavier review

Some code carries signing, wire-shape, or spend authority. Changes under these
paths get one adversarial review pass and explicit maintainer sign-off before
merge, regardless of how small the diff looks — the same discipline the project
used before it went public:

- `sentinel/sovereign-protocol/**` — signing / JCS canonicalization
- `**/comms*`, the escalation/directive envelope, outbox and capability-token code
- `.github/workflows/**` and CI/build scripts

If your change touches one of these, say so in the PR description and expect
questions. Everything else follows the normal review path.

## Test fixtures that look like secrets

The security tests intentionally contain realistic-looking fake credentials
(API keys, tokens, a sample JWT) so they can exercise the redaction and
detection code. These are allow-listed for the secret scanner and must stay
shaped like the real thing. If you add a **new** example in docs, use an
obvious placeholder such as `<YOUR_API_KEY>` instead — never a realistic-looking
value. See `.gitleaks.toml` for the current allow-list.

## Reporting security issues

**Do not** open a public issue for a vulnerability. Report privately via GitHub's
"Report a vulnerability" button under the repository's **Security** tab (private
security advisory). Details and the disclosure expectations are in
[SECURITY.md](SECURITY.md). Contact: soc@irinity.com.

## Questions

For usage questions and ideas, open a GitHub issue (label `question` if you
like). Discussions are not enabled on this repo yet. For how a specific piece
works, the subtree `README.md` and `docs/` directories are the source of truth.
