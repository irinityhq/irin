# IRIN release runbook

How a public IRIN release is produced. One transaction, fail-closed, operator-run.
Support matrix: **macOS on Apple silicon (arm64) only** — Intel Macs are not
supported, and there is no Windows/Linux desktop build.

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

Credentials live only in the operator's login keychain and shell environment
(`APPLE_SIGNING_IDENTITY`, `APPLE_NOTARY_PROFILE`). Nothing credential-bearing
is committed or read into agent context.

## Apple distribution boundary

IRIN uses two separate Apple-supported development/distribution paths:

- The macOS product is distributed directly as a Developer ID-signed,
  Hardened Runtime, timestamped, notarized, and stapled DMG. Apple describes
  notarization as an automated security scan, not App Review.
- War Room on Dave's iPhone is a private registered-device development build.
  It is not an App Store, TestFlight, Enterprise, or public IPA distribution.

Before a signed iPhone build, use Xcode's current privacy report to audit the
compiled app and linked SDKs, keep `PrivacyInfo.xcprivacy` limited to justified
required-reason APIs, and confirm the phone is a trusted registered
destination. A cable is needed once if trust/registration or wireless pairing
is not already established.

Primary references, checked 2026-07-24:

- [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Distributing to registered devices](https://developer.apple.com/documentation/xcode/distributing-your-app-to-registered-devices)
- [Required-reason APIs](https://developer.apple.com/documentation/bundleresources/describing-use-of-required-reason-api)
- [Apple Developer Program License Agreement](https://developer.apple.com/support/terms/apple-developer-program-license-agreement/)

## The transaction

### Pre-merge release-candidate proof

`scripts/release-transaction.sh --dry-run-rc` is not a no-effect simulation. It
builds and pushes mutable `rc-<source-sha>` Gateway Pack images to GHCR, resolves
their live digests, creates a production-signed DMG, and submits it to Apple's
notary service before stapling and verification. It does not create, upload to,
or publish a GitHub Release. The lane therefore requires working GHCR package
write access, Docker, the Developer ID identity, and the notary profile, and it
leaves external RC image and Apple notarization records behind.

### Post-merge release

After the product PR merges:

```bash
git checkout main && git pull            # exact merged source
export IRIN_RELEASE_VERSION=0.1.2
git tag "v$IRIN_RELEASE_VERSION" <merged SHA>
git push origin "v$IRIN_RELEASE_VERSION"
# release.yml creates the DRAFT release (Linux council binary)
# release-images.yml publishes the matching immutable GHCR version

export APPLE_SIGNING_IDENTITY="Developer ID Application: <Name> (<TEAM_ID>)"
export APPLE_NOTARY_PROFILE="irin-notary"
scripts/release-transaction.sh --tag "v$IRIN_RELEASE_VERSION"
```

The ladder, in order, each step fail-closed: clean-tree and identity preflight
(refuses dirty tree, missing Developer ID, unusable notary profile,
`IRIN_SMOKE_APP` substitution, remapped `HOME`, app-support isolation) →
registry-pinned production manifest → production DMG build (Developer ID,
hardened runtime, notarization, staple) → untouched-copy verification bound to
the explicitly named `HASHES.txt` (DMG and bundled binary/resource hashes,
packaging mode, version, identity, Gatekeeper, staple) → `PROMOTION=1` smoke on
the untouched DMG using the same receipt → attach the **exact accepted bytes**
and `HASHES.txt` to
the draft release. Manifest generation, production staging/build, and the
untouched-copy verifier each replay the exact digest refs against GHCR: both
image revision annotations must equal the release commit and the sidecar must
carry the production-only release-eligibility annotation. These gates require
read access to the public GHCR packages and fail closed on registry errors.

Then the operator performs native acceptance on the notarized DMG (fresh
install, first run + migration continuity, Keychain/Touch ID, real Direct
deliberation, no-Docker behavior, Gateway Pack enable → governed deliberation
→ Watch/Outbox truthful and disarmed → explicit Touch ID arm/renew/disarm →
private tailnet phone access → relaunch persistence → disable/re-enable →
uninstall/reinstall), re-downloads the asset from the draft, compares the
checksum, installs, launches — and only then publishes the release. The iPhone
shell then gets its separate registered-device install and one zero-provider
run against the accepted Mac build before a live-provider phone run.

## Rollback

A bad draft is deleted before publication. A bad published release is marked
withdrawn and its assets are removed, but the immutable tag and version are
never reused; the fix ships under the next patch version. The public website
only points at a release after re-download verification, so rollback includes
removing or replacing that download link.

## First-run notes for operators upgrading from "Council War Room"

- The app is now **IRIN** (`com.irinity.irin`). Existing Application Support
  state is copied forward on first launch; the legacy directory is never
  deleted by migration. Keychain items from the legacy identity are copied
  when ACL permits; otherwise Gateway Pack Enable re-provisions.
- Bundle-identity change resets macOS accessibility/automation (TCC) grants;
  the first run asks again.
- An old `Council War Room.app` and the new `IRIN.app` can coexist; the old
  one is left untouched — delete it yourself when ready.
