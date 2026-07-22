# Security Policy

## Reporting a Vulnerability

Report vulnerabilities privately through GitHub Security Advisories for this
repository. Do not open a public issue for a vulnerability. Reports may also be
sent to `soc@irinity.com`.

Include the affected commit, reproduction steps, expected and observed
behavior, and any relevant logs with credentials and private content removed.

## Supported Version

The `main` branch is the only supported development line before the first
stable release.

## Deployment Boundary

IRIN is local-first software for one operator.

- Council, War Room Web, and Gateway bind to loopback by default.
- Tailscale access is an operator-controlled private overlay, not a public
  deployment mode.
- Gateway authentication is fail-closed when caller keys are not provisioned.
- The watch producer and action path remain disabled unless explicitly enabled
  and armed.
- Authenticated worker-management routes are mounted, but the built-in worker
  loop is disabled by default and is not an operator-ready autonomous
  execution path.
- IRIN makes no compliance, certification, or conformance claim.

Do not expose the Council API, Gateway, WebSocket, or watch endpoints directly
to an untrusted network.

## Secrets and Local State

Provider credentials, Gateway caller keys, signing keys, and Tailscale identity
must remain outside the repository. The local setup uses `~/.config/irin/`,
`~/.irin/`, and Docker volumes. Files containing credentials must be mode 0600
where the platform supports POSIX permissions.

Council sessions and Gateway records can contain operator prompts, model
responses, evidence, and signed artifacts. Secret-shaped values are scrubbed on
selected ingestion paths, but IRIN does not promise general content redaction.
Protect the host and its backups accordingly.

## Trust Limits

The hash chain detects mutation of recorded Gateway events. Ed25519 signatures
allow offline verification of artifacts produced by the configured signing
key. Neither mechanism proves that a host was uncompromised at signing time.
An attacker with host-level access may read local content, replace binaries,
or use software-held credentials.

Hardware-backed arming adds a separate confirmation boundary where configured,
but it is not a substitute for host security.

## Dependencies

CI runs secret scanning, Rust advisory checks, license policy checks, and SBOM
generation for both the root Rust workspace and the standalone Tauri desktop
workspace. New actionable advisories fail CI. The following reviewed
exceptions remain visible in audit output and are tracked until their upstream
dependency constraints move:

- `RUSTSEC-2024-0436` is an unmaintained compile-time dependency inherited
  through the root embedding stack.
- `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` affect `quick-xml` APIs that the
  locked Tauri consumers do not call. The macOS path reads the app bundle's
  local `Info.plist` through `plist`, which uses plain `quick_xml::Reader`
  without namespace resolution or attribute iteration. The second locked copy
  belongs to the unsupported Windows notification target.
- `RUSTSEC-2024-0429` affects `glib::VariantStrIter` in Tauri's Linux GTK tree.
  IRIN does not call that API and does not ship the native shell on Linux;
  Ubuntu uses the browser War Room. A separately built Linux Tauri shell keeps
  the upstream risk and is not a supported release artifact.

## Security-Relevant Source

- `gateway/sidecar-rs/src/watch/` contains watch, outbox, arming, and signing
  paths.
- `sentinel/sovereign-protocol/` contains shared wire types and canonical JSON
  behavior.
- `gateway/COUNCIL_GATEWAY_CONTRACT.md` documents the Gateway/Council boundary.
- `sentinel/COMMS_CONTRACT.md` documents Escalation and Directive envelopes.
- `docs/security-claims-vs-reality.md` states which claims are enforced,
  partial, disabled, or not shipped.
