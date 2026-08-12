#!/usr/bin/env bash
# PR #77 identity-boundary contracts for live install-verify.
#
# Covers:
#   1) Hermetic mode never resolves to real /Applications without a temp fixture dest
#   2) Nested Mach-O DevID binding is wired (shared helper + call site)
#   3) Non-canonical candidate.json is refused; id uses canonical bytes
#
# Hermetic only — no real /Applications mutation, no Apple notary.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL="$ROOT/scripts/install-verify-candidate.sh"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

failures=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

# Exact-path cleanup only: never glob /tmp (concurrent runs share the prefix).
CLEANUP_DIRS=()
cleanup() {
  local d
  for d in ${CLEANUP_DIRS[@]+"${CLEANUP_DIRS[@]}"}; do
    rm -rf "$d"
  done
}
trap cleanup EXIT
# Sets TMPDIR_LAST (no command substitution: a subshell could not append to
# the parent's CLEANUP_DIRS array).
tmpdir() {
  TMPDIR_LAST="$(mktemp -d "$1")"
  CLEANUP_DIRS+=("$TMPDIR_LAST")
}

finish() {
  if (( failures > 0 )); then
    printf 'install-identity-boundaries: FAILED (%d)\n' "$failures" >&2
    exit 1
  fi
  printf 'install-identity-boundaries: OK\n'
  exit 0
}

[[ -f "$INSTALL" ]] || fail "missing install-verify-candidate.sh"
[[ -f "$ROOT/packaging/codesign-identity.sh" ]] || fail "missing packaging/codesign-identity.sh"

# ---------------------------------------------------------------------------
# 1) Hermetic Applications root: fixture dest required; never real /Applications
# ---------------------------------------------------------------------------
IRIN_INSTALL_VERIFY_LIB=1
# shellcheck source=/dev/null
source "$INSTALL"
unset IRIN_INSTALL_VERIFY_LIB

export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_CANDIDATE_ROOT
# Force /tmp — TMPDIR may be inside the checkout (candidate-root refuse).
tmpdir /tmp/irin-id-cand.XXXXXX
IRIN_CANDIDATE_ROOT="$TMPDIR_LAST"
HERMETIC_CAND_ROOT="$TMPDIR_LAST"
unset IRIN_LIVE_APPLICATIONS_ROOT IRIN_LIVE_REQUIRE_DEVELOPER_ID 2>/dev/null || true

set +e
out="$(resolve_live_applications_root 2>&1)"
ec=$?
set -e
if [[ $ec -ne 0 ]] && [[ "$out" == *"IRIN_LIVE_APPLICATIONS_ROOT"* || "$out" == *"temp root"* || "$out" == *"/Applications"* ]]; then
  pass "hermetic without fixture Applications root refuses (not real /Applications)"
else
  fail "hermetic without fixture dest must refuse (got ec=$ec out=$out)"
fi

# Explicit real /Applications override refused under hermetic
export IRIN_LIVE_APPLICATIONS_ROOT=/Applications
set +e
out="$(resolve_live_applications_root 2>&1)"
ec=$?
set -e
if [[ $ec -ne 0 ]] && [[ "$out" == *"/Applications"* ]]; then
  pass "hermetic refuses IRIN_LIVE_APPLICATIONS_ROOT=/Applications"
else
  fail "hermetic must refuse real /Applications override (got ec=$ec out=$out)"
fi

# Temp fixture dest accepted
export IRIN_LIVE_APPLICATIONS_ROOT
tmpdir /tmp/irin-id-apps.XXXXXX
IRIN_LIVE_APPLICATIONS_ROOT="$TMPDIR_LAST"
set +e
apps="$(resolve_live_applications_root 2>&1)"
ec=$?
set -e
if [[ $ec -eq 0 && "$apps" != "/Applications" ]] && path_under_temp_root "$apps"; then
  pass "hermetic accepts temp fixture Applications root"
else
  fail "hermetic temp fixture dest failed (ec=$ec apps=$apps)"
fi

# Invalid hermetic config must refuse, never fall through to /Applications:
# flag set with a NON-temp candidate root...
export IRIN_CANDIDATE_ROOT="$ROOT"
set +e
out="$(resolve_live_applications_root 2>&1)"
ec=$?
set -e
if [[ $ec -ne 0 && "$out" != "/Applications" ]]; then
  pass "hermetic flag with non-temp candidate root refuses (no /Applications fall-through)"
else
  fail "hermetic flag + non-temp candidate root must refuse (ec=$ec out=$out)"
fi
# ...and flag set with the candidate root unset.
set +e
out="$(IRIN_CANDIDATE_ROOT='' resolve_live_applications_root 2>&1)"
ec=$?
set -e
if [[ $ec -ne 0 && "$out" != "/Applications" ]]; then
  pass "hermetic flag with unset candidate root refuses"
else
  fail "hermetic flag + unset candidate root must refuse (ec=$ec out=$out)"
fi
IRIN_CANDIDATE_ROOT="$HERMETIC_CAND_ROOT"
export IRIN_CANDIDATE_ROOT

# DevID skip only for fixture dest (not bare hermetic)
if hermetic_fixture_applications_active; then
  pass "hermetic_fixture_applications_active true with temp override"
else
  fail "hermetic_fixture_applications_active should be true with temp override"
fi
unset IRIN_LIVE_APPLICATIONS_ROOT
if hermetic_fixture_applications_active; then
  fail "hermetic_fixture_applications_active must be false without override"
else
  pass "hermetic_fixture_applications_active false without override"
fi

# ---------------------------------------------------------------------------
# 2) Nested Mach-O binding is shared and called from install-verify
# ---------------------------------------------------------------------------
if grep -q 'irin_assert_nested_developer_id_identity' "$INSTALL" \
  && grep -q 'codesign-identity.sh' "$INSTALL"; then
  pass "install-verify wires nested DevID helper (codesign-identity.sh)"
else
  fail "install-verify must call irin_assert_nested_developer_id_identity via codesign-identity.sh"
fi

# Incomplete inventory refuses (executable helper contract)
tmpdir /tmp/irin-id-app.XXXXXX
fake_app="$TMPDIR_LAST/IRIN.app"
mkdir -p "$fake_app/Contents/MacOS"
printf 'not-macho' >"$fake_app/Contents/MacOS/council-warroom-tauri"
# shellcheck source=/dev/null
source "$ROOT/packaging/codesign-identity.sh"
set +e
inv_out="$(irin_assert_expected_macho_inventory "$fake_app" 2>&1)"
inv_ec=$?
set -e
if [[ $inv_ec -ne 0 ]]; then
  pass "incomplete Mach-O inventory refuses (shared helper)"
else
  fail "incomplete Mach-O inventory must refuse: $inv_out"
fi

# Library mode is sourced-only: direct execution must refuse, not exit 0.
set +e
IRIN_INSTALL_VERIFY_LIB=1 bash "$INSTALL" >/dev/null 2>&1
lib_ec=$?
set -e
if [[ $lib_ec -ne 0 ]]; then
  pass "direct execution refuses IRIN_INSTALL_VERIFY_LIB=1"
else
  fail "IRIN_INSTALL_VERIFY_LIB=1 must not be a direct-execution success bypass"
fi

# ---------------------------------------------------------------------------
# 2b) Identity-violation refusals execute (stubbed codesign)
# ---------------------------------------------------------------------------
# The nested binding must refuse a non-Developer-ID Authority and a
# TeamIdentifier mismatch — not merely be wired. Stub codesign so both
# violation paths execute without real signing material.
run_identity_stub() {
  # Distinct name: bash dynamic scoping would otherwise resolve this to
  # verify_developer_id_signature's own local `details` inside the stub.
  local stub_details="$1"
  (
    die() { exit 70; }
    log() { :; }
    # shellcheck source=/dev/null
    source "$ROOT/packaging/codesign-identity.sh"
    codesign() {
      case "$1" in
        --verify) return 0 ;;
        -dv) printf '%s\n' "$stub_details" ;;
        -d) return 0 ;;
        *) return 1 ;;
      esac
    }
    verify_developer_id_signature "/dev/null" "TEAM123" "stub artifact" >/dev/null 2>&1
  )
}

good_details='Authority=Developer ID Application: Stub
CodeDirectory v=20500 flags=0x10000(runtime)
Timestamp=Jun 1, 2026 at 12:00:00 PM
TeamIdentifier=TEAM123'

set +e
run_identity_stub "$good_details"
ok_ec=$?
run_identity_stub "${good_details/Developer ID Application/Apple Development}"
auth_ec=$?
run_identity_stub "${good_details/TeamIdentifier=TEAM123/TeamIdentifier=EVIL999}"
team_ec=$?
set -e
if [[ $ok_ec -eq 0 ]]; then
  pass "stubbed conforming signature accepted (control)"
else
  fail "stub control signature must pass (ec=$ok_ec)"
fi
if [[ $auth_ec -ne 0 ]]; then
  pass "nested non-Developer-ID Authority refused"
else
  fail "non-Developer-ID Authority must refuse"
fi
if [[ $team_ec -ne 0 ]]; then
  pass "nested TeamIdentifier mismatch refused"
else
  fail "TeamIdentifier mismatch must refuse"
fi

# Entitlement inspection failure must refuse, not read as "no entitlements".
run_identity_stub_ent_fail() {
  (
    die() { exit 70; }
    log() { :; }
    # shellcheck source=/dev/null
    source "$ROOT/packaging/codesign-identity.sh"
    codesign() {
      case "$1" in
        --verify) return 0 ;;
        -dv) printf '%s\n' "$good_details" ;;
        -d) return 1 ;;
        *) return 1 ;;
      esac
    }
    verify_developer_id_signature "/dev/null" "TEAM123" "stub artifact" >/dev/null 2>&1
  )
}
set +e
run_identity_stub_ent_fail
ent_ec=$?
set -e
if [[ $ent_ec -ne 0 ]]; then
  pass "entitlements inspection failure refused (fail closed)"
else
  fail "entitlements inspection failure must refuse"
fi

# ---------------------------------------------------------------------------
# 3) Non-canonical candidate.json refused; canonical form accepted for id match
# ---------------------------------------------------------------------------
if [[ "$(uname -s)" != "Darwin" ]]; then
  pass "live-entry identity contracts skipped on non-Darwin (hdiutil/ditto)"
  finish
fi
# Build a minimal store candidate under /tmp (not under checkout).
tmpdir /tmp/irin-id-store.XXXXXX
store="$TMPDIR_LAST"
export IRIN_CANDIDATE_ROOT="$store"
export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_LIVE_APPLICATIONS_ROOT
tmpdir /tmp/irin-id-liveapps.XXXXXX
IRIN_LIVE_APPLICATIONS_ROOT="$TMPDIR_LAST"
export IRIN_STATE_ROOT
tmpdir /tmp/irin-id-state.XXXXXX
IRIN_STATE_ROOT="$TMPDIR_LAST"

sha="$(python3 -c 'print("ab"*32)')"
tmpdir /tmp/irin-id-stage.XXXXXX
stage="$TMPDIR_LAST"
mkdir -p "$stage/proofs" "$stage/install/IRIN.app/Contents/MacOS" \
  "$stage/install/IRIN.app/Contents/Helpers" "$stage/logs"
printf 'host' >"$stage/install/IRIN.app/Contents/MacOS/council-warroom-tauri"
printf 'side' >"$stage/install/IRIN.app/Contents/MacOS/council"
printf 'arm' >"$stage/install/IRIN.app/Contents/Helpers/arm-attest"
irin_write_bundle_manifest "$stage/install/IRIN.app" "$stage/bundle-manifest.txt"
bm_d="$(irin_sha256_file "$stage/bundle-manifest.txt")"
# Minimal DMG for attach path (or stub on non-Darwin).
dmg="$stage/IRIN_0.0.0_test.dmg"
mkdir -p "$stage/dmgroot"
ditto "$stage/install/IRIN.app" "$stage/dmgroot/IRIN.app"
if [[ "$(uname -s)" == "Darwin" ]]; then
  hdiutil create -volname IRIN -srcfolder "$stage/dmgroot" -ov -format UDZO "$dmg" >/dev/null
else
  printf 'not-a-dmg' >"$dmg"
fi
dmg_d="$(irin_sha256_file "$dmg")"

# Non-canonical JSON (pretty-print) but store id = hash(raw)
python3 - "$stage/candidate.json" "$sha" "$bm_d" "$dmg_d" <<'PY'
import json, sys
out, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.0.0-test",
  "pack_mode": "signed-rc",
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": False,
  "gateway_digest": "g" + ("0" * 63),
  "sidecar_digest": "s" + ("0" * 63),
}
open(out, "w", encoding="utf-8").write(json.dumps(doc, indent=2) + "\n")
PY
raw_id="$(irin_sha256_file "$stage/candidate.json")"
cat >"$stage/HASHES.txt" <<EOF
pack_mode=signed-rc
release_version=0.0.0-test
releasable=false
stapled=false
source_sha=$sha
bundle_manifest_digest=$bm_d
dmg_sha256=$dmg_d
EOF
dest="$store/0.0.0-test/$sha/$raw_id"
mkdir -p "$(dirname "$dest")"
rm -rf "$dest"
mv "$stage" "$dest"

set +e
noncanon_out="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_ROOT="$store" \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  IRIN_STATE_ROOT="$IRIN_STATE_ROOT" \
  bash "$INSTALL" --candidate "$dest" --live 2>&1
)"
noncanon_ec=$?
set -e
if [[ $noncanon_ec -ne 0 ]] && [[ "$noncanon_out" == *"canonical"* ]]; then
  pass "non-canonical candidate.json refused before live swap"
else
  fail "non-canonical candidate.json must refuse (ec=$noncanon_ec out=$noncanon_out)"
fi

# Canonical rewrite: same object, canonical bytes, id = hash(canon)
tmpdir /tmp/irin-id-canon.XXXXXX
canon_stage="$TMPDIR_LAST"
cp -R "$dest/." "$canon_stage/"
irin_canonical_identity_json <"$dest/candidate.json" >"$canon_stage/candidate.json"
canon_id="$(irin_sha256_file "$canon_stage/candidate.json")"
canon_dest="$store/0.0.0-test/$sha/$canon_id"
rm -rf "$canon_dest"
mkdir -p "$(dirname "$canon_dest")"
mv "$canon_stage" "$canon_dest"
set +e
canon_out="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_ROOT="$store" \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  IRIN_STATE_ROOT="$IRIN_STATE_ROOT" \
  bash "$INSTALL" --candidate "$canon_dest" --live 2>&1
)"
canon_ec=$?
set -e
if [[ "$canon_out" == *"canonical identity form"* ]]; then
  fail "canonical candidate.json must not hit non-canonical refuse: $canon_out"
elif [[ "$canon_out" == *"does not recompute from candidate.json"* ]]; then
  fail "canonical candidate-id must match store path: $canon_out"
elif [[ "$canon_out" != *"fresh-extract DMG into install/"* ]]; then
  # The gate passes silently; the fresh-extract note is the first post-gate
  # output, so its presence proves the identity gate actually cleared.
  fail "canonical candidate.json must clear the identity gate to fresh-extract (ec=$canon_ec): $canon_out"
else
  pass "canonical candidate.json clears identity gate (ec=$canon_ec)"
fi

# ---------------------------------------------------------------------------
# 4) Entry-point identity refusals: --live reaches the nested DevID gate
# ---------------------------------------------------------------------------
# Drive install-verify --live with PATH-stubbed codesign/file so the Authority
# and TeamIdentifier refusals fire through the production entry point, not
# only the shared helper. Asserting on the gate's own messages proves the
# refusal came from the identity gate, not an earlier failure.
tmpdir /tmp/irin-id-stub.XXXXXX
stubbin="$TMPDIR_LAST"
cat >"$stubbin/file" <<'STUB'
#!/usr/bin/env bash
printf 'Mach-O 64-bit executable arm64\n'
STUB
chmod +x "$stubbin/file"

write_codesign_stub() {
  # $1 = outer-app team, $2 = nested-binary team, $3 = Authority line
  cat >"$stubbin/codesign" <<STUB
#!/usr/bin/env bash
outer_team='$1'; nested_team='$2'; authority='$3'
case "\$1" in
  --verify) exit 0 ;;
  -dv)
    artifact="\${*: -1}"
    team="\$nested_team"
    case "\$artifact" in *.app) team="\$outer_team" ;; esac
    printf 'Authority=%s\n' "\$authority"
    printf 'CodeDirectory v=20500 flags=0x10000(runtime)\n'
    printf 'Timestamp=Jun 1, 2026 at 12:00:00 PM\n'
    printf 'TeamIdentifier=%s\n' "\$team"
    exit 0 ;;
  -d) exit 0 ;;
esac
exit 1
STUB
  chmod +x "$stubbin/codesign"
}

run_live_with_stub() {
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_ROOT="$store" \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  IRIN_STATE_ROOT="$IRIN_STATE_ROOT" \
  IRIN_LIVE_REQUIRE_DEVELOPER_ID=1 \
  PATH="$stubbin:$PATH" \
  bash "$INSTALL" --candidate "$canon_dest" --live 2>&1
}

write_codesign_stub TEAM123 TEAM123 'Apple Development: Stub'
set +e
ep_auth_out="$(run_live_with_stub)"
ep_auth_ec=$?
set -e
if [[ $ep_auth_ec -ne 0 && "$ep_auth_out" == *"Developer ID Application"* ]]; then
  pass "--live entry point refuses non-Developer-ID Authority (stubbed codesign)"
else
  fail "--live must refuse non-DevID Authority at the gate (ec=$ep_auth_ec out=$ep_auth_out)"
fi

write_codesign_stub TEAM123 EVIL999 'Developer ID Application: Stub'
set +e
ep_team_out="$(run_live_with_stub)"
ep_team_ec=$?
set -e
if [[ $ep_team_ec -ne 0 && "$ep_team_out" == *"TeamIdentifier"* ]]; then
  pass "--live entry point refuses nested TeamIdentifier mismatch (stubbed codesign)"
else
  fail "--live must refuse nested TeamIdentifier mismatch at the gate (ec=$ep_team_ec out=$ep_team_out)"
fi

finish
