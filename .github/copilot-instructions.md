# IRIN code review

Review changed behavior for correctness, security, contract preservation, and
proof quality. Report actionable defects; skip praise, summaries, style
preferences, and speculative redesigns.

- Trace each behavior change through the authoritative runtime path. Flag tests
  that exercise only a helper, source spelling, mock path, or disconnected
  fixture instead of the production caller.
- Preserve public, wire, and storage contracts unless the pull request
  explicitly changes them. Treat `sentinel/sovereign-protocol/**`, signing,
  arming, directive envelopes, the outbox, capability tokens, and workflows as
  contract-sensitive; verify sibling paths remain fail-closed.
- At trust boundaries, check authentication, authorization, input validation,
  secret handling, path containment, rollback, and error handling. A failure
  must not silently become success, zero, or an unauthenticated fallback.
- Prefer the existing authoritative workflow, state machine, gate, and receipt
  path. Flag parallel control planes, compatibility shims, or abstractions that
  duplicate an existing mechanism.
- Check GitHub Actions changes for trigger reachability, least-privilege
  permissions, full-SHA action pins, pagination or truncation fail-open
  behavior, and whether a claimed required check runs on the reviewed revision.
- Bind conclusions to the pull request head and actual diff. Verify
  documentation and pull-request claims against source and runnable checks.
- Keep proof boundaries exact: source, tests, build, package, install, operator
  acceptance, and publication are separate states. `make check` and
  `make ship-check` prove source only.
- Prefer the smallest durable fix at the shared root cause. Ask for broader
  work only when the demonstrated defect requires it.

Use `docs/architecture.md`, `docs/security-claims-vs-reality.md`,
`docs/development-workflow.md`, `CONTRIBUTING.md`, and the affected component
README for repository context. Never request or expose credentials, private
runtime state, or machine-local configuration.
