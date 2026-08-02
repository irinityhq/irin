# IRIN release runbook

How a public IRIN release is produced. Two non-overlapping actions, fail-closed,
operator-run. Support matrix: **macOS on Apple silicon (arm64) only** — Intel
Macs are not supported, and there is no Windows/Linux desktop build.

## One-time setup (operator)

1. **Apple Developer Program** membership (paid) with access to team
   `irinityhq` work. Verify the Team ID.
2. **Developer ID Application certificate**: Xcode → Settings → Accounts →
   select the team → Manage Certificates → "+" → *Developer ID Application*.
   Confirm with `security find-identity -v -p codesigning` — an
   `Authority=Developer ID Application: …` identity must exist.
3. **Notary profile**: create an app-specific password at appleid.apple.com,
   then
   `xcrun notarytool store-credentials "irin-notary" --apple-id <apple-id> --team-id <TEAM_ID> --password <app-specific-password>`
4. **GHCR package visibility** (browser, once): after the first image publish,
   flip both `irin-gateway` and `irin-sidecar` packages to **public** under the
   irinityhq org. Anonymous digest resolution and user pulls depend on it.
5. **Current Apple terms and tools**: accept any pending agreement in the
   developer portal/Xcode before the release gate. The policy snapshot must be
   refreshed after an Apple agreement or Xcode major change and at least every
   30 days.
6. **Ship board (optional operator surface)**: durable home is
   `~/.local/share/irin/ship-board`. In every IRIN worktree run
   `make link-ship-board` (or `scripts/link-ship-board.sh`) so
   `tools/ship-board` points at that home. The board **calls**
   `scripts/candidate-status.sh --json` and never reimplements tiers.

Credentials live only in the operator's login keychain and shell environment
(`APPLE_SIGNING_IDENTITY`, `APPLE_NOTARY_PROFILE`). Nothing credential-bearing
is committed or read into agent context.

## Apple distribution boundary

The macOS product is distributed directly as a Developer ID-signed, Hardened
Runtime, timestamped, notarized, and stapled DMG. Apple describes notarization
as an automated security scan, not App Review. Private phone access uses the
same War Room web export over Tailscale Serve in a browser on the operator's
tailnet — there is no separate iOS product surface.

Primary references, checked 2026-07-24:

- [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Apple Developer Program License Agreement](https://developer.apple.com/support/terms/apple-developer-program-license-agreement/)

## Five tiers (evidence only)

| Tier | Meaning |
| --- | --- |
| Source integrated | Required source checks + exact commit on `main` |
| Candidate verified | Immutable candidate from that SHA passed automated proof |
| Installed | Installed bytes == named candidate under bundle-manifest |
| Accepted | Dave accepted those exact production bytes (T2) |
| Published | Public GH asset is the accepted candidate |

`scripts/candidate-status.sh` is the **sole** reporter of candidate-derived
tiers. Merge means Source integrated only. Ambiguous "done/ready/shipped"
without tier + identity is invalid speech.

## The transaction

### 1) T1 — prepare production (no GitHub Release)

After Phase B signed-rc biometry/visual and a valid T1 packet:

```bash
# Write T1 packet (board CLI or hand-authored JSON)
irin-mission write-t1-packet \
  --out /tmp/t1.json \
  --signed-rc-id <64-char-candidate-id> \
  --source-sha <40-char-merged-sha> \
  --attempt-id prep-$(date -u +%Y%m%dT%H%M%SZ) \
  --expiry 2099-01-01T00:00:00Z

export APPLE_SIGNING_IDENTITY="Developer ID Application: <Name> (<TEAM_ID>)"
export APPLE_NOTARY_PROFILE="irin-notary"

scripts/release-transaction.sh \
  --prepare-production \
  --t1-packet /tmp/t1.json
```

This is **not** a no-effect simulation. It:

1. Runs bounded preflight (clean tree, free credentials, no remapped HOME)
2. Pushes mutable `rc-<sha12>` Gateway Pack images to GHCR
3. Resolves live digests into a production image manifest
4. Builds one production-mode signed/notarized/stapled DMG into
   `IRIN_CANDIDATE_ROOT` and prints `candidate_path=`
5. Verifies and promotion-smokes that candidate

It does **not** create, upload to, or publish a GitHub Release. A temporary
`--dry-run-rc` alias still works but prints the same irreversible effects and
requires the same T1 packet.

### 2) Install proof + T2 acceptance

```bash
CANDIDATE=<absolute candidate_path from prepare>

scripts/install-verify-candidate.sh --candidate "$CANDIDATE"
# → proofs/install.json  (digests only; not Arm/Watch product proof)

# Board creates a pending T2 action (not yet authorization)
irin-mission create-pending-t2 \
  --candidate "$CANDIDATE" \
  --expiry 2099-01-01T00:00:00Z

# Interactive only — phrase must include full source SHA + DMG hash +
# installed bundle-manifest digest
scripts/record-acceptance.sh \
  --candidate "$CANDIDATE" \
  --installed-app "$CANDIDATE/install/IRIN.app"
# → proofs/acceptance.json then proofs/t2.json (one-way; no rewrite)
```

Caveat: Accepted does not cryptographically prove who typed a structurally
valid receipt. The human boundary is the operator-controlled T2 action.

### 3) Publish (publication only — never rebuilds)

```bash
scripts/release-transaction.sh \
  --publish \
  --tag "v$IRIN_RELEASE_VERSION" \
  --candidate "$CANDIDATE" \
  --t2-packet "$CANDIDATE/proofs/t2.json"
```

Publication:

1. Requires production pack mode, version/tag/source equality, Installed proof,
   and final T2 acceptance (`candidate-status --require Accepted`)
2. Promotes the candidate's exact Gateway/sidecar digest refs to immutable
   `vX.Y.Z` labels and re-resolves both labels before the git tag push
3. Pushes the git tag; waits for the draft release (`release.yml` may attach
   the Linux Council binary only — no workflow attaches a desktop DMG)
4. Uploads the exact candidate DMG **without** `--clobber` (equal hash =
   idempotent skip; different hash = hard refuse)
5. Authenticated draft re-download proves upload integrity only
6. Under T2, publishes the release, fetches the public asset **without**
   authentication, verifies the accepted post-staple hash, and writes
   `proofs/publication.json`

`release-images.yml` is **not** tag-triggered. Version labels are applied from
the candidate during `--publish`. Any retained manual image-build lane is
outside publication authority.

### Status

```bash
scripts/candidate-status.sh --candidate "$CANDIDATE" --json
# or
make candidate-status ARGS="--candidate $CANDIDATE --require Published"
```

Ship board (after `make link-ship-board` + `select-candidate`) renders the same
JSON; it never derives tiers from `packaging/artifacts/` or raw PASS text.

## Rollback

A bad draft is deleted before publication. A bad published release is marked
withdrawn and its assets are removed, but the immutable tag and version are
never reused; the fix ships under the next patch version as a **superseding**
candidate. A published defective candidate is never replaced under its tag or
asset name. Site deploy is outside Published scope until it has its own
re-download proof.

## First-run notes for operators upgrading from "Council War Room"

- The app is now **IRIN** (`com.irinity.irin`). Existing Application Support
  state is copied forward on first launch; the legacy directory is never
  deleted by migration. Keychain items from the legacy identity are copied
  when ACL permits; otherwise Gateway Pack Enable re-provisions.
- Bundle-identity change resets macOS accessibility/automation (TCC) grants;
  the first run asks again.
- An old `Council War Room.app` and the new `IRIN.app` can coexist; the old
  one is left untouched — delete it yourself when ready.
