# packaging/codesign-identity.sh — shared Developer ID / Mach-O inventory helpers.
#
# Used by packaging/verify-dmg.sh and scripts/install-verify-candidate.sh so
# live install and DMG verify bind the same nested identity contract.
#
# Caller must define die(). Optional log() defaults to printf.
# shellcheck shell=bash

if ! declare -F log >/dev/null 2>&1; then
  log() { printf '%s\n' "$*"; }
fi

irin_macho_inventory() {
  local app="$1" candidate
  while IFS= read -r -d '' candidate; do
    if file -b "$candidate" 2>/dev/null | grep -q '^Mach-O'; then
      printf '%s\n' "${candidate#"$app"/}"
    fi
  done < <(find "$app/Contents" -type f -print0 2>/dev/null)
}

irin_assert_expected_macho_inventory() {
  local app="$1" actual expected
  actual="$(irin_macho_inventory "$app" | LC_ALL=C sort)"
  expected="$(printf '%s\n' \
    'Contents/Helpers/arm-attest' \
    'Contents/MacOS/council' \
    'Contents/MacOS/council-warroom-tauri' | LC_ALL=C sort)"
  [[ "$actual" == "$expected" ]] || {
    printf 'Expected Mach-O inventory:\n%s\nActual Mach-O inventory:\n%s\n' \
      "$expected" "$actual" >&2
    die "unexpected Mach-O inventory in app bundle"
  }
}

# Alias names used by verify-dmg (kept for call-site clarity).
macho_inventory() { irin_macho_inventory "$@"; }
assert_expected_macho_inventory() { irin_assert_expected_macho_inventory "$@"; }

verify_developer_id_signature() {
  local artifact="$1" expected_team="$2" label="$3" details entitlements team
  codesign --verify --strict "$artifact" \
    || die "$label failed strict signature verification"
  # Authority display strings are signer-controlled; require an Apple-anchored
  # Developer ID chain so a self-signed certificate named "Developer ID
  # Application: ..." cannot satisfy this helper.
  codesign --verify --strict \
    -R='anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists' \
    "$artifact" \
    || die "$label does not satisfy the Apple-anchored Developer ID requirement"
  details="$(codesign -dv --verbose=4 "$artifact" 2>&1)" \
    || die "could not inspect $label signature"
  [[ "$details" == *"Authority=Developer ID Application"* ]] \
    || die "$label is not signed with Developer ID Application"
  grep -Eq '^CodeDirectory .*flags=.*runtime' <<<"$details" \
    || die "$label is missing the Hardened Runtime signature flag"
  grep -q '^Timestamp=' <<<"$details" \
    || die "$label is missing a trusted signing timestamp"
  ! grep -q '^Timestamp=none$' <<<"$details" \
    || die "$label has no trusted signing timestamp"
  team="$(awk -F= '$1 == "TeamIdentifier" { print $2; exit }' <<<"$details")"
  [[ -n "$team" && "$team" == "$expected_team" ]] \
    || die "$label TeamIdentifier does not match the outer app"
  entitlements="$(codesign -d --entitlements :- "$artifact" 2>/dev/null || true)"
  if grep -q '<key>' <<<"$entitlements"; then
    die "$label contains entitlements, but IRIN declares none; review and document before shipping"
  fi
  log "$label signature: Developer ID, runtime, timestamp, TeamIdentifier, no entitlements"
}

# Full nested binding used by live install and DMG verify for signed-rc/production.
irin_assert_nested_developer_id_identity() {
  local app="$1" outer_details outer_team
  assert_expected_macho_inventory "$app"
  outer_details="$(codesign -dv --verbose=4 "$app" 2>&1)" \
    || die "could not inspect outer app signature"
  [[ "$outer_details" == *"Authority=Developer ID Application"* ]] \
    || die "outer app is not signed with Developer ID Application"
  outer_team="$(awk -F= '$1 == "TeamIdentifier" { print $2; exit }' <<<"$outer_details")"
  [[ -n "$outer_team" ]] || die "outer app signature has no TeamIdentifier"
  verify_developer_id_signature "$app" "$outer_team" "outer app"
  verify_developer_id_signature "$app/Contents/Helpers/arm-attest" "$outer_team" "Touch ID helper"
  verify_developer_id_signature "$app/Contents/MacOS/council" "$outer_team" "Council sidecar"
  verify_developer_id_signature \
    "$app/Contents/MacOS/council-warroom-tauri" "$outer_team" "Tauri host"
}
