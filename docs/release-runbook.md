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

Credentials live only in the operator's login keychain and shell environment
(`APPLE_SIGNING_IDENTITY`, `APPLE_NOTARY_PROFILE`). Nothing credential-bearing
is committed or read into agent context.

## The transaction

After the product PR merges:

```bash
git checkout main && git pull            # exact merged source
git tag v0.1.0 <merged SHA> && git push origin v0.1.0
# release.yml creates the DRAFT release (Linux council binary)
# release-images.yml publishes ghcr.io/irinityhq/{irin-gateway,irin-sidecar}:v0.1.0

export APPLE_SIGNING_IDENTITY="Developer ID Application: <Name> (<TEAM_ID>)"
export APPLE_NOTARY_PROFILE="irin-notary"
scripts/release-transaction.sh --tag v0.1.0
```

The ladder, in order, each step fail-closed: clean-tree and identity preflight
(refuses dirty tree, missing Developer ID, unusable notary profile,
`IRIN_SMOKE_APP` substitution, remapped `HOME`, app-support isolation) →
registry-pinned production manifest → production DMG build (Developer ID,
hardened runtime, notarization, staple) → untouched-copy verification
(identity, Gatekeeper, staple) → `PROMOTION=1` smoke on the untouched DMG →
checksums + receipt → attach the **exact accepted bytes** and `HASHES.txt` to
the draft release.

Then the operator performs native acceptance on the notarized DMG (fresh
install, first run + migration continuity, Keychain/Touch ID, real Direct
deliberation, no-Docker behavior, Gateway Pack enable → governed deliberation
→ Watch/Outbox truthful and disarmed → relaunch persistence →
disable/re-enable → uninstall/reinstall), re-downloads the asset from the
draft, compares the checksum, installs, launches — and only then publishes the
release.

## Rollback

A bad release is rolled back by deleting the draft or yanking the published
release and deleting the tag. The public website only points at a release
after re-download verification, so a rollback never leaves a public download
referencing pulled bytes.

## First-run notes for operators upgrading from "Council War Room"

- The app is now **IRIN** (`com.irinity.irin`). Existing Application Support
  state is copied forward on first launch; the legacy directory is never
  deleted by migration. Keychain items from the legacy identity are copied
  when ACL permits; otherwise Gateway Pack Enable re-provisions.
- Bundle-identity change resets macOS accessibility/automation (TCC) grants;
  the first run asks again.
- An old `Council War Room.app` and the new `IRIN.app` can coexist; the old
  one is left untouched — delete it yourself when ready.
