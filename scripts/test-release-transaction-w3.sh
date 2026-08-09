#!/usr/bin/env bash
# Hermetic W3 contracts for prepare/publish refuses + install/acceptance.
# Zero network, zero Apple, zero provider. Uses a fake-gh when needed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-w3-tx-test.XXXXXX")"
cleanup() {
  if [[ -d "$TEST_HOME" ]]; then
    chmod -R u+w "$TEST_HOME" 2>/dev/null || true
    rm -rf "$TEST_HOME"
  fi
}
trap cleanup EXIT

export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates"
# Hermetic live Applications + state roots (never touch real /Applications).
export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_LIVE_APPLICATIONS_ROOT="$TEST_HOME/Applications"
export IRIN_STATE_ROOT="$TEST_HOME/state"
mkdir -p "$IRIN_LIVE_APPLICATIONS_ROOT" "$IRIN_STATE_ROOT"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

TX="$ROOT/scripts/release-transaction.sh"
INSTALL="$ROOT/scripts/install-verify-candidate.sh"
ACCEPT="$ROOT/scripts/record-acceptance.sh"
STATUS="$ROOT/scripts/candidate-status.sh"
[[ -x "$TX" && -x "$INSTALL" && -x "$ACCEPT" && -x "$STATUS" ]] \
  || fail "W3 scripts not executable"

LIVE_APP="$IRIN_LIVE_APPLICATIONS_ROOT/IRIN.app"

# Seed hermetic daily-use app from a candidate (matching bytes).
seed_live_app_from() {
  local src_app="$1"
  rm -rf "$LIVE_APP"
  mkdir -p "$IRIN_LIVE_APPLICATIONS_ROOT"
  cp -R "$src_app" "$LIVE_APP"
  chmod -R u+w "$LIVE_APP" 2>/dev/null || true
}

sha40() { python3 -c "print(('$1' * 40)[:40])"; }

make_staging() {
  local staging="$1" pack_mode="$2" source_sha="$3" dmg_body="$4" stapled="$5"
  rm -rf "$staging"
  mkdir -p "$staging/IRIN.app/Contents/MacOS" \
    "$staging/proofs" "$staging/smoke" "$staging/install" "$staging/logs"
  printf 'host' >"$staging/IRIN.app/Contents/MacOS/council-warroom-tauri"
  printf 'side' >"$staging/IRIN.app/Contents/MacOS/council"
  local dmg_name="IRIN_0.1.2_aarch64.dmg"
  printf '%s' "$dmg_body" >"$staging/$dmg_name"
  irin_write_bundle_manifest "$staging/IRIN.app" "$staging/bundle-manifest.txt"
  local bm_d dmg_d app_d
  bm_d="$(irin_sha256_file "$staging/bundle-manifest.txt")"
  dmg_d="$(irin_sha256_file "$staging/$dmg_name")"
  app_d="$(irin_sha256_file "$staging/IRIN.app/Contents/MacOS/council-warroom-tauri")"
  cat >"$staging/HASHES.txt" <<EOF
pack_mode=$pack_mode
release_version=0.1.2
releasable=$([ "$stapled" = "true" ] && echo true || echo false)
stapled=$stapled
source_sha=$source_sha
build_dirty=false
arch=aarch64-apple-darwin
app=IRIN.app
dmg=$dmg_name
app_sha256=$app_d
council_sha256=$(irin_sha256_file "$staging/IRIN.app/Contents/MacOS/council")
arm_attest_sha256=$(printf 'x' | irin_sha256_bytes)
gateway_pack_compose_sha256=$(printf 'y' | irin_sha256_bytes)
gateway_pack_manifest_sha256=$(printf 'z' | irin_sha256_bytes)
gateway_digest=$(python3 -c 'print("g"+"0"*63)')
sidecar_digest=$(python3 -c 'print("s"+"0"*63)')
warroom_web_index_sha256=$(printf 'w' | irin_sha256_bytes)
bundle_manifest_digest=$bm_d
dmg_sha256=$dmg_d
EOF
  python3 - "$staging/candidate.json" "$pack_mode" "$source_sha" "$bm_d" "$dmg_d" "$stapled" <<'PY'
import json, sys
out, pack_mode, source_sha, bm_d, dmg_d, stapled = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.1.2",
  "pack_mode": pack_mode,
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": stapled == "true",
  "gateway_digest": "g" + ("0" * 63),
  "sidecar_digest": "s" + ("0" * 63),
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
}

write_proof() {
  local path="$1" kind="$2" cid="$3" sha="$4" result="$5" extra="${6-}"
  [[ -n "$extra" ]] || extra='{}'
  python3 - "$path" "$kind" "$cid" "$sha" "$result" "$extra" <<'PY'
import json, sys, uuid
from datetime import datetime, timezone
path, kind, cid, sha, result, extra_raw = sys.argv[1:]
extra = json.loads(extra_raw)
doc = {
  "schema_version": 1,
  "proof_kind": kind,
  "candidate_id": cid,
  "source_sha": sha,
  "result": result,
  "tool_version": "irin-test/1",
  "run_id": str(uuid.uuid4()),
  "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
}
for k, v in extra.items():
    doc[k] = v
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY
}

# --- prepare refuses without T1 packet -------------------------------------
set +e
out="$("$TX" --prepare-production 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "prepare without T1 should refuse"
[[ "$out" == *"t1-packet"* || "$out" == *"T1"* || "$out" == *"required"* ]] \
  || fail "expected T1 refuse message: $out"
pass "prepare-production refuses without T1 packet"

# --- dry-run-rc name is retired (not a silent no-op or alias) --------------
# Require retirement wording + migration target. Flag-name alone is not enough:
# generic "unknown argument: --dry-run-rc" would also contain the flag.
set +e
out="$("$TX" --dry-run-rc 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "retired --dry-run-rc must refuse"
[[ "$out" == *"removed"* ]] \
  || fail "expected retirement wording (removed), not generic unknown-arg: $out"
[[ "$out" == *"--prepare-production"* ]] \
  || fail "expected migration to --prepare-production: $out"
pass "retired --dry-run-rc refuses with remove+migrate contract"

# --- publish without --candidate dies --------------------------------------
set +e
out="$("$TX" --publish --tag v0.1.2 --t2-packet /tmp/nope 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "publish without --candidate should die"
[[ "$out" == *"--candidate"* || "$out" == *"candidate"* ]] \
  || fail "expected candidate requirement: $out"
pass "publish without --candidate dies"

# --- promote a production candidate below Accepted -------------------------
SHA="$(sha40 c)"
S1="$TEST_HOME/stage-prod"
make_staging "$S1" "production" "$SHA" "dmg-prod-bytes" "true"
CID="$(irin_sha256_file "$S1/candidate.json")"
DEST="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA/$CID"
irin_promote_candidate_from_staging "$S1" "$DEST" >/dev/null
chmod -R u+w "$DEST/proofs" "$DEST/install" 2>/dev/null || true
BM_D="$(irin_sha256_file "$DEST/bundle-manifest.txt")"
DMG_D="$(python3 -c 'import json; print(json.load(open("'"$DEST"'/candidate.json"))["dmg_sha256"])')"
EXTRA="$(python3 -c 'import json; print(json.dumps({
  "dmg_sha256": "'"$DMG_D"'",
  "bundle_manifest_digest": "'"$BM_D"'",
}))')"
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "PASS" "$EXTRA"

export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true

# Only Candidate verified — publish must refuse.
set +e
out="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$TX" --publish --tag v0.1.2 --candidate "$DEST" \
    --t2-packet "$DEST/proofs/t2.json" 2>&1
)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "publish at Candidate verified should refuse"
[[ "$out" == *"Accepted"* || "$out" == *"refuse"* || "$out" == *"below"* || "$out" == *"require"* || "$out" == *"missing"* ]] \
  || fail "expected Accepted refuse: $out"
pass "publish refuses Candidate verified (below Accepted)"

# --- install-verify: digest match path (copy app simulates extract) --------
# Real hdiutil needs a real DMG; for hermetic unit test, simulate install tree
# the same way candidate-status tests do, then invoke install-verify only when
# hdiutil can attach. Here we prove the failure path: diverging digests.
mkdir -p "$DEST/install"
cp -R "$DEST/IRIN.app" "$DEST/install/IRIN.app"
chmod -R u+w "$DEST/install" 2>/dev/null || true
# Mutate installed app so digests diverge when re-manifested.
printf 'MUTATED' >"$DEST/install/IRIN.app/Contents/MacOS/council"
# Directly exercise the digest compare logic via candidate-status after a forged install.json
INST_EXTRA="$(python3 -c 'import json; print(json.dumps({
  "candidate_bundle_manifest_digest": "'"$BM_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
}))')"
write_proof "$DEST/proofs/install.json" "install" "$CID" "$SHA" "PASS" "$INST_EXTRA"
# Forged install proof with mutated tree must not yield Installed.
tier="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$STATUS" --candidate "$DEST" --json \
  | python3 -c 'import json,sys; t=json.load(sys.stdin).get("tier"); print(t if t else "")'
)"
[[ "$tier" == "Candidate verified" || -z "$tier" || "$tier" == "Candidate verified" ]] \
  || fail "mutated install must not be Installed (got '$tier')"
# status may stay at Candidate verified when install_proof invalid
pass "install proof with diverging installed bytes does not yield Installed"

# --- record-acceptance refuses non-tty -------------------------------------
# Restore install tree for later steps.
rm -rf "$DEST/install"
mkdir -p "$DEST/install"
cp -R "$DEST/IRIN.app" "$DEST/install/IRIN.app"
chmod -R u+w "$DEST/install" 2>/dev/null || true
irin_write_bundle_manifest "$DEST/install/IRIN.app" "$DEST/install/bundle-manifest.txt"
write_proof "$DEST/proofs/install.json" "install" "$CID" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "candidate_bundle_manifest_digest": "'"$BM_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
}))')"
seed_live_app_from "$DEST/IRIN.app"
# Fresh acceptance needs pending-t2 present before the tty gate.
printf '%s\n' '{
  "schema_version": 1,
  "packet_kind": "pending-t2",
  "action_id": "t2-test-action",
  "candidate_id": "'"$CID"'",
  "source_sha": "'"$SHA"'",
  "authorized_effects": ["tag-push", "release-attach", "publish", "version-image-labels"],
  "expiry": "2099-01-01T00:00:00Z"
}' >"$DEST/proofs/pending-t2.json"
rm -f "$DEST/proofs/acceptance.json" "$DEST/proofs/t2.json"

set +e
out="$(printf 'nope\n' | "$ACCEPT" --candidate "$DEST" --installed-app "$LIVE_APP" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "piped record-acceptance should refuse"
[[ "$out" == *"tty"* || "$out" == *"interactive"* ]] \
  || fail "expected tty refuse: $out"
pass "record-acceptance refuses non-tty / piped input"

# Production pin: non-/Applications path refused outside hermetic live override.
# Also: non-hermetic ignores IRIN_LIVE_APPLICATIONS_ROOT (containment folded here).
set +e
out_pin="$(
  IRIN_CANDIDATE_STATUS_HERMETIC= \
  IRIN_LIVE_APPLICATIONS_ROOT= \
  "$ACCEPT" --candidate "$DEST" --installed-app "$DEST/install/IRIN.app" 2>&1
)"
ec_pin=$?
set -e
[[ $ec_pin -ne 0 ]] || fail "production acceptance must refuse candidate-local install path: $out_pin"
[[ "$out_pin" == *"/Applications/IRIN.app"* || "$out_pin" == *"production acceptance requires"* ]] \
  || fail "expected /Applications pin refuse: $out_pin"
set +e
out_pin2="$(
  IRIN_CANDIDATE_STATUS_HERMETIC= \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  "$ACCEPT" --candidate "$DEST" --installed-app "$LIVE_APP" 2>&1
)"
ec_pin2=$?
set -e
[[ $ec_pin2 -ne 0 ]] || fail "non-hermetic must ignore live Applications override: $out_pin2"
[[ "$out_pin2" == *"/Applications/IRIN.app"* || "$out_pin2" == *"production acceptance requires"* ]] \
  || fail "expected /Applications pin when override ignored: $out_pin2"
pass "production acceptance refuses non-/Applications path outside hermetic mode"

# --- pending-t2 + acceptance fields: digest mismatch refuses even if we had tty
# Pending already written above.
# Wrong installed app content
WRONG="$TEST_HOME/wrong.app"
rm -rf "$WRONG"
mkdir -p "$WRONG/Contents/MacOS"
printf 'wrong' >"$WRONG/Contents/MacOS/x"
# Use a path named IRIN.app with wrong bytes
WRONG_APP="$TEST_HOME/IRIN.app"
rm -rf "$WRONG_APP"
mkdir -p "$WRONG_APP/Contents/MacOS"
printf 'wrong-host' >"$WRONG_APP/Contents/MacOS/council-warroom-tauri"
printf 'wrong-side' >"$WRONG_APP/Contents/MacOS/council"

# script checks tty first; use a pseudo-tty via python if available, else
# verify the digest check is in the script.
grep -q 'installed-app digest mismatch\|bundle-manifest' "$ACCEPT" \
  || fail "record-acceptance must check installed-app digest"
grep -q 'interactive tty\|requires an interactive tty' "$ACCEPT" \
  || fail "record-acceptance must require tty"
pass "record-acceptance encodes tty + installed-app digest mismatch refuses"

# --- prepare with invalid T1 packet refuses --------------------------------
BAD_T1="$TEST_HOME/bad-t1.json"
printf '%s\n' '{"schema_version":1,"packet_kind":"t1"}' >"$BAD_T1"
set +e
out="$("$TX" --prepare-production --t1-packet "$BAD_T1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "invalid T1 should refuse"
pass "prepare refuses incomplete T1 packet"

# --- T1 length-only IDs that are non-hex must refuse (#0041) ----------------
# Error text already claimed hex; validator used to check only 64/40 length.
NONHEX_CID="$(python3 -c 'print("g" * 64)')"
NONHEX_SHA="$(python3 -c 'print("z" * 40)')"
HEX_CID="$(python3 -c 'print("a" * 64)')"
HEX_SHA="$(python3 -c 'print("b" * 40)')"
EFFECTS='["ghcr-rc-push","apple-rc-notarization","one-production-cycle"]'
BAD_CID_T1="$TEST_HOME/bad-t1-nonhex-cid.json"
BAD_SHA_T1="$TEST_HOME/bad-t1-nonhex-sha.json"
python3 - "$BAD_CID_T1" "$NONHEX_CID" "$HEX_SHA" "$EFFECTS" <<'PY'
import json, sys
path, cid, sha, effects = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
json.dump({
    "schema_version": 1,
    "packet_kind": "t1",
    "signed_rc_candidate_id": cid,
    "source_sha": sha,
    "production_attempt_id": "attempt-nonhex-cid",
    "authorized_effects": effects,
    "expiry": "2099-01-01T00:00:00Z",
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
python3 - "$BAD_SHA_T1" "$HEX_CID" "$NONHEX_SHA" "$EFFECTS" <<'PY'
import json, sys
path, cid, sha, effects = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
json.dump({
    "schema_version": 1,
    "packet_kind": "t1",
    "signed_rc_candidate_id": cid,
    "source_sha": sha,
    "production_attempt_id": "attempt-nonhex-sha",
    "authorized_effects": effects,
    "expiry": "2099-01-01T00:00:00Z",
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
set +e
out="$("$TX" --prepare-production --t1-packet "$BAD_CID_T1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "non-hex candidate id of length 64 must refuse"
[[ "$out" == *"hex"* || "$out" == *"signed_rc_candidate_id"* ]] \
  || fail "expected hex candidate refuse: $out"
set +e
out="$("$TX" --prepare-production --t1-packet "$BAD_SHA_T1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "non-hex source_sha of length 40 must refuse"
[[ "$out" == *"hex"* || "$out" == *"source_sha"* ]] \
  || fail "expected hex source_sha refuse: $out"
pass "T1 refuses non-hex candidate/source IDs at correct length"

# --- [P1] gh asset lookup uses jq --arg, never gh --arg --------------------
# Static: no gh --arg in the publish path (gh does not support it).
if grep -nE 'gh (release view|api) .*--arg' "$TX"; then
  fail "release-transaction must not pass --arg to gh (use jq --arg)"
fi
grep -q 'jq -r --arg' "$TX" || fail "expected jq --arg for asset name filtering"
pass "publish asset lookup uses jq --arg (not gh --arg)"

# --- [P1] remote tag peel + lookup failure (real helpers) ------------------
grep -q 'remote_tag_peeled_commit' "$TX" || fail "missing remote_tag_peeled_commit"
grep -qF 'refs/tags/${tag}^{}' "$TX" || fail "peel helper must request refs/tags/\$tag^{}"
# Must not swallow ls-remote failures.
if grep -n 'ls-remote' "$TX" | grep -q '|| true'; then
  fail "remote_tag_peeled_commit must not use || true on ls-remote"
fi
# Source the real script as a library (helpers only).
# shellcheck disable=SC1090
IRIN_RELEASE_TX_LIB=1 source "$TX"

PEEL_HOME="$TEST_HOME/peel-remote"
rm -rf "$PEEL_HOME"
mkdir -p "$PEEL_HOME"
git init --bare -q "$PEEL_HOME/remote.git"
git -C "$PEEL_HOME" init -q work
git -C "$PEEL_HOME/work" config user.email "test@example.com"
git -C "$PEEL_HOME/work" config user.name "test"
printf 'body\n' >"$PEEL_HOME/work/f"
git -C "$PEEL_HOME/work" add f
git -C "$PEEL_HOME/work" commit -q -m "c"
COMMIT_SHA="$(git -C "$PEEL_HOME/work" rev-parse HEAD)"
git -C "$PEEL_HOME/work" remote add origin "$PEEL_HOME/remote.git"
git -C "$PEEL_HOME/work" tag -a v0.0.99 -m "annotated test"
git -C "$PEEL_HOME/work" push -q origin "refs/tags/v0.0.99"
# Single-pattern still returns tag-object only (documents the bug class).
SINGLE_SHA="$(git -C "$PEEL_HOME/work" ls-remote --tags origin "refs/tags/v0.0.99" | awk '{print $1; exit}')"
[[ "$SINGLE_SHA" != "$COMMIT_SHA" ]] \
  || fail "expected annotated tag-object SHA != commit for single-pattern ls-remote"
GOT_PEEL="$(
  cd "$PEEL_HOME/work" && remote_tag_peeled_commit v0.0.99 origin
)"
[[ "$GOT_PEEL" == "$COMMIT_SHA" ]] \
  || fail "remote_tag_peeled_commit returned $GOT_PEEL want $COMMIT_SHA (tag-object was $SINGLE_SHA)"
# Successful lookup, tag absent → empty, exit 0.
ABSENT="$(
  cd "$PEEL_HOME/work" && remote_tag_peeled_commit v9.9.9 origin
)"
[[ -z "$ABSENT" ]] || fail "absent tag should yield empty peel, got $ABSENT"
# Lookup failure (bad remote) must refuse — not empty/absent.
set +e
FAIL_OUT="$(
  cd "$PEEL_HOME/work" && remote_tag_peeled_commit v0.0.99 no-such-remote-xyz 2>&1
)"
FAIL_EC=$?
set -e
[[ $FAIL_EC -ne 0 ]] || fail "ls-remote failure must be non-zero, not treated as absent: $FAIL_OUT"
[[ "$FAIL_OUT" == *"ls-remote failed"* || "$FAIL_OUT" == *"ERROR"* ]] \
  || fail "expected lookup-failure refuse message: $FAIL_OUT"
pass "remote peel + lookup failure refuse (real helper)"

# --- [P1] public retry uses allow_create=0 for labels ----------------------
grep -q 'allow_create' "$TX" || fail "promote_version_labels must support allow_create"
# Public branch must call with 0; draft path with 1
grep -n 'promote_version_labels' "$TX" | grep -q ' 0' \
  || fail "public path must call promote_version_labels … 0"
grep -n 'RELEASE_STATE.*public\|release already public' "$TX" >/dev/null \
  || fail "must branch on already-public release before mutation"
pass "public-release retry is validation-only for labels"

# --- [P1] attempt effects ledger: complete effects are skipped -------------
grep -q 'effects' "$TX" || fail "attempt receipt must track effects"
grep -q 'status.*complete\|"status": "complete"' "$TX" || fail "effect status complete missing"
grep -q 'production-build was interrupted without a candidate path' "$TX" \
  || fail "interrupted notary must refuse silent re-spend"
# Interrupted GHCR without both digests must refuse — never fall through to push.
grep -q 'ghcr-rc-push was interrupted without both SHA-bound digests recoverable' "$TX" \
  || fail "interrupted GHCR must refuse re-push"
if grep -q 'completing once' "$TX"; then
  fail "must not re-invoke push under same T1 when digests unrecoverable"
fi
grep -q 'not eligible for a fresh push' "$TX" \
  || fail "fresh push must be gated on empty effect status"
pass "attempt ledger refuses interrupted GHCR re-push and silent re-notary"

# --- #0056 production-cycle + checkout binding -----------------------------
grep -q 'production_cycle_consumed\|production-cycle-' "$TX" \
  || fail "prepare must track production-cycle consumption per source SHA"
grep -q 'notarization already consumed\|authorize a T3 exception' "$TX" \
  || fail "second production cycle must refuse without T3"
grep -q 'validate_t3_exception\|--t3-exception' "$TX" \
  || fail "prepare must accept --t3-exception for a second cycle"
grep -q 'reserve_production_cycle' "$TX" \
  || fail "must exclusive-reserve production cycle before external effects"
grep -q 'snapshot_checkout_control' "$TX" \
  || fail "must snapshot checkout HEAD + scripts/packaging dirtiness"
grep -q 'checkout_head' "$TX" || fail "attempt receipt must record checkout_head"
grep -q 'scripts_dirty' "$TX" || fail "attempt receipt must record scripts_dirty"
grep -q 'packaging_dirty' "$TX" || fail "attempt receipt must record packaging_dirty"
# Both publish safeguards must be present independently (no OR alternation).
grep -q 'publish requires checkout HEAD' "$TX" \
  || fail "publish must require same checkout HEAD"
grep -q 'publish requires clean scripts' "$TX" \
  || fail "publish must require clean scripts/packaging"
# Cycle claim must be scheduled before GHCR push effect.
python3 - "$TX" <<'PY' || fail "production-cycle claim must precede ghcr-rc-push effect"
from pathlib import Path
import sys
text = Path(sys.argv[1]).read_text(encoding="utf-8")
# Locate do_prepare body roughly via unique markers.
i_claim = text.find("production-cycle claim before first irreversible effect")
i_ghcr = text.find('# ---- effect: ghcr-rc-push (once)')
if i_claim < 0 or i_ghcr < 0 or i_claim > i_ghcr:
    raise SystemExit(1)
sys.exit(0)
PY
# Live helper: cycle ledger + T3 single-use + recover + CAS (#0056/#0058)
(
  set -euo pipefail
  # shellcheck disable=SC1090
  IRIN_RELEASE_TX_LIB=1 source "$TX"
  export IRIN_CANDIDATE_ROOT="$TEST_HOME/cycle-root"
  mkdir -p "$IRIN_CANDIDATE_ROOT/.attempts/t3-spent"
  CYCLE_SHA="$(python3 -c 'print("c" * 40)')"
  HEAD="$CYCLE_SHA"
  write_t3() {
    local path="$1" sha="$2" words="$3"
    python3 - "$path" "$sha" "$words" <<'PY'
import json, sys
path, sha, words = sys.argv[1:]
json.dump({
    "schema_version": 1,
    "packet_kind": "t3",
    "source_sha": sha,
    "words": words,
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
  }
  # First cycle: exclusive reserve + consume with clean T1 bind
  reserve_production_cycle "$CYCLE_SHA" "attempt-1" "" "$HEAD" "false" "false" >/dev/null
  [[ "$(production_cycle_state "$CYCLE_SHA")" == "reserved" ]]
  record_production_cycle_consumed "$CYCLE_SHA" "attempt-1" "$TEST_HOME/cand-a"
  production_cycle_consumed "$CYCLE_SHA" || {
    printf 'cycle must report consumed after record\n' >&2
    exit 1
  }
  # Malformed ledger must fail closed
  BAD_LEDGER="$(production_cycle_path "$CYCLE_SHA")"
  printf '%s\n' '{"kind":"production-cycle","source_sha":"deadbeef"}' >"$BAD_LEDGER"
  set +e
  bad_out="$(production_cycle_state "$CYCLE_SHA" 2>&1)"
  bad_ec=$?
  set -e
  [[ $bad_ec -ne 0 ]] || {
    printf 'malformed ledger must die, got: %s\n' "$bad_out" >&2
    exit 1
  }
  # Restore consumed with bind fields for T3 path
  python3 - "$BAD_LEDGER" "$CYCLE_SHA" <<'PY'
import json, sys
path, sha = sys.argv[1:]
json.dump({
    "schema_version": 1,
    "kind": "production-cycle",
    "source_sha": sha,
    "status": "consumed",
    "notarization_consumed": True,
    "production_attempt_id": "attempt-1",
    "checkout_head": sha,
    "scripts_dirty": False,
    "packaging_dirty": False,
    "spent_t3_digests": [],
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
  # Empty / object words refuse
  BAD_T3="$TEST_HOME/bad-t3.json"
  printf '%s\n' "{\"schema_version\":1,\"packet_kind\":\"t3\",\"source_sha\":\"$CYCLE_SHA\",\"words\":\"\"}" >"$BAD_T3"
  set +e
  validate_t3_exception "$BAD_T3" "$CYCLE_SHA" >/dev/null 2>&1
  t3_ec=$?
  set -e
  [[ $t3_ec -ne 0 ]] || { printf 'empty T3 words must refuse\n' >&2; exit 1; }
  OBJ_T3="$TEST_HOME/obj-t3.json"
  python3 - "$OBJ_T3" "$CYCLE_SHA" <<'PY'
import json, sys
path, sha = sys.argv[1:]
json.dump({
    "schema_version": 1,
    "packet_kind": "t3",
    "source_sha": sha,
    "words": {"apple": sha},
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
  set +e
  validate_t3_exception "$OBJ_T3" "$CYCLE_SHA" >/dev/null 2>&1
  t3_ec=$?
  set -e
  [[ $t3_ec -ne 0 ]] || { printf 'object T3 words must refuse\n' >&2; exit 1; }
  # Second cycle with T3-A
  T3A="$TEST_HOME/t3-a.json"
  write_t3 "$T3A" "$CYCLE_SHA" "Authorize second apple notary cycle for source $CYCLE_SHA"
  DIGEST_A="$(validate_t3_exception "$T3A" "$CYCLE_SHA")"
  reserve_production_cycle "$CYCLE_SHA" "attempt-2" "$T3A" "$HEAD" "false" "false" >/dev/null
  [[ "$(production_cycle_state "$CYCLE_SHA")" == "reserved" ]]
  record_production_cycle_consumed "$CYCLE_SHA" "attempt-2" "$TEST_HOME/cand-b"
  # Same T3-A must not authorize a third cycle
  set +e
  reuse_out="$(reserve_production_cycle "$CYCLE_SHA" "attempt-3" "$T3A" "$HEAD" "false" "false" 2>&1)"
  reuse_ec=$?
  set -e
  [[ $reuse_ec -ne 0 ]] || {
    printf 'reused T3 must refuse third cycle, got: %s\n' "$reuse_out" >&2
    exit 1
  }
  [[ "$reuse_out" == *"already spent"* ]] || {
    printf 'expected spent message, got: %s\n' "$reuse_out" >&2
    exit 1
  }
  # Foreign reserved recovery requires T3; without T3 refuses
  python3 - "$(production_cycle_path "$CYCLE_SHA")" "$CYCLE_SHA" "$DIGEST_A" <<'PY'
import json, sys
path, sha, dig = sys.argv[1:]
json.dump({
    "schema_version": 1,
    "kind": "production-cycle",
    "source_sha": sha,
    "status": "reserved",
    "notarization_consumed": False,
    "production_attempt_id": "attempt-zombie",
    "checkout_head": sha,
    "scripts_dirty": False,
    "packaging_dirty": False,
    "spent_t3_digests": [dig],
}, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
  set +e
  fr_out="$(reserve_production_cycle "$CYCLE_SHA" "attempt-4" "" "$HEAD" "false" "false" 2>&1)"
  fr_ec=$?
  set -e
  [[ $fr_ec -ne 0 ]] || {
    printf 'foreign reserved without T3 must refuse\n' >&2
    exit 1
  }
  # Fresh T3-B recovers abandoned reserved
  T3B="$TEST_HOME/t3-b.json"
  write_t3 "$T3B" "$CYCLE_SHA" "Authorize abandoned recovery apple cycle for source $CYCLE_SHA"
  reserve_production_cycle "$CYCLE_SHA" "attempt-4" "$T3B" "$HEAD" "false" "false" >/dev/null
  [[ "$(production_cycle_state "$CYCLE_SHA")" == "reserved" ]]
  # Wrong SHA T3
  WRONG_T3="$TEST_HOME/wrong-t3.json"
  write_t3 "$WRONG_T3" "$(python3 -c 'print("d"*40)')" \
    "Authorize second apple notary cycle for source $(python3 -c 'print("d"*40)')"
  set +e
  validate_t3_exception "$WRONG_T3" "$CYCLE_SHA" >/dev/null 2>&1
  t3_ec=$?
  set -e
  [[ $t3_ec -ne 0 ]] || { printf 'T3 for wrong SHA must refuse\n' >&2; exit 1; }
) || fail "production-cycle/T3 live helper failed"
pass "production-cycle ledger + T3 exception + checkout binding (#0056)"
# Static: T3 spent + flock serialization + publish uses recorded dirty
grep -q 't3-spent\|t3_packet_sha256\|spent_t3_digests' "$TX" \
  || fail "T3 must be single-use by digest"
grep -q 'fcntl.flock\|LOCK_EX' "$TX" || fail "ledger transitions must use flock"
grep -q 'refuse unknowable history\|lacks checkout_head' "$TX" \
  || fail "legacy receipts without bind fields must refuse"
grep -q 'recorded scripts_dirty must be false\|scripts_dirty must be false at T1' "$TX" \
  || fail "publish must require recorded T1 dirty flags false"
pass "production-cycle serialization + T3 single-use + publish T1 bind (#0058)"

# --- [P1] prepare requires Candidate verified + runtime preflight ----------
grep -q 'Candidate verified' "$TX" || fail "prepare must require Candidate verified"
grep -q 'preflight_runtime_bounds\|free :8765\|port :8765' "$TX" \
  || fail "prepare must check free :8765"
grep -q 'council-warroom-tauri\|IRIN process' "$TX" \
  || fail "prepare must refuse running IRIN process"
pass "prepare requires Candidate verified + process/port preflight"

# --- [P1] publish waits for release.yml — real helper + fake gh ------------
grep -q 'wait_for_tag_release_workflow' "$TX" || fail "missing wait_for_tag_release_workflow"
grep -q 'IRIN_GH_RUNS_JSON' "$TX" || fail "workflow selector must take JSON via env (not stdin pipe+heredoc)"
# SC2259 class: must not pipe into python with a program heredoc.
if grep -nE 'printf.*runs.*\|.*python3|runs.*\|.*python3' "$TX" | grep -v '^#' | grep -q .; then
  # Allow only if no heredoc on same construct — hard-fail the old pattern.
  if grep -n 'printf.*runs' "$TX" | grep -q 'python3'; then
    fail "must not pipe runs JSON into python3 (use IRIN_GH_RUNS_JSON)"
  fi
fi
python3 - "$TX" <<'PY' || fail "workflow wait must precede draft attach"
import sys
text = open(sys.argv[1]).read()
pub = text.split("do_publish()")[1]
assert 0 < pub.find("wait_for_tag_release_workflow") < pub.find("draft release: upload DMG")
print("workflow-before-draft order ok")
PY

# Source helpers already done above when peel tests ran; re-source if needed.
# shellcheck disable=SC1090
IRIN_RELEASE_TX_LIB=1 source "$TX"

WANT_SHA="$(printf 'c%.0s' {1..40})"
TAG_WF="v0.1.2"
# Real select_tag_release_run with env JSON (the fixed path).
SUCCESS_JSON="$(python3 -c 'import json; print(json.dumps([
  {"databaseId": 1, "headSha": "d"*40, "status": "completed", "conclusion": "success",
   "headBranch": "v0.1.2", "event": "push", "displayTitle": "IRIN Release", "name": "x"},
  {"databaseId": 99, "headSha": "'"$(printf 'c%.0s' {1..40})"'", "status": "completed",
   "conclusion": "success", "headBranch": "v0.1.2", "event": "push",
   "displayTitle": "IRIN Release", "name": "x"},
]))')"
eval "$(IRIN_GH_RUNS_JSON="$SUCCESS_JSON" select_tag_release_run "$TAG_WF" "$WANT_SHA")"
[[ "${MATCHED:-0}" == "1" ]] || fail "select_tag_release_run should match success run"
[[ "$RUN_ID" == "99" ]] || fail "expected run 99, got $RUN_ID"
[[ "$RUN_CONCLUSION" == "success" ]] || fail "expected success conclusion"

EMPTY_JSON='[]'
eval "$(IRIN_GH_RUNS_JSON="$EMPTY_JSON" select_tag_release_run "$TAG_WF" "$WANT_SHA")"
[[ "${MATCHED:-0}" == "0" ]] || fail "empty run list must not match"

# Real wait_for_tag_release_workflow with fake gh on PATH.
FAKEBIN="$TEST_HOME/fakebin"
rm -rf "$FAKEBIN"
mkdir -p "$FAKEBIN"
GH_RUNS_FILE="$TEST_HOME/gh-runs.json"
printf '%s\n' "$SUCCESS_JSON" >"$GH_RUNS_FILE"
cat >"$FAKEBIN/gh" <<'EOF'
#!/usr/bin/env bash
# Minimal fake gh: only "run list" is used by the waiter.
if [[ "$1" == "run" && "$2" == "list" ]]; then
  cat "${IRIN_TEST_GH_RUNS_FILE:?}"
  exit 0
fi
echo "fake-gh: unexpected $*" >&2
exit 2
EOF
chmod +x "$FAKEBIN/gh"
export IRIN_TEST_GH_RUNS_FILE="$GH_RUNS_FILE"
export IRIN_RELEASE_WORKFLOW_WAIT_ATTEMPTS=3
export IRIN_RELEASE_WORKFLOW_WAIT_SLEEP=0
set +e
WAIT_OUT="$(
  PATH="$FAKEBIN:$PATH" \
  wait_for_tag_release_workflow "$TAG_WF" "$WANT_SHA" 2>&1
)"
WAIT_EC=$?
set -e
[[ $WAIT_EC -eq 0 ]] || fail "wait_for_tag_release_workflow should succeed with matching run: $WAIT_OUT"
[[ "$WAIT_OUT" == *"succeeded"* || "$WAIT_OUT" == *"99"* ]] \
  || fail "expected success note from real waiter: $WAIT_OUT"

# Wrong SHA → timeout (no match), not false success.
WRONG_SHA="$(printf 'a%.0s' {1..40})"
set +e
WAIT_OUT2="$(
  PATH="$FAKEBIN:$PATH" \
  IRIN_RELEASE_WORKFLOW_WAIT_ATTEMPTS=2 \
  IRIN_RELEASE_WORKFLOW_WAIT_SLEEP=0 \
  wait_for_tag_release_workflow "$TAG_WF" "$WRONG_SHA" 2>&1
)"
WAIT_EC2=$?
set -e
[[ $WAIT_EC2 -ne 0 ]] || fail "waiter must not succeed for wrong SHA: $WAIT_OUT2"
[[ "$WAIT_OUT2" == *"timed out"* || "$WAIT_OUT2" == *"no release.yml run"* ]] \
  || fail "expected timeout/no-run for wrong SHA: $WAIT_OUT2"

# Failed conclusion must refuse immediately.
FAIL_JSON="$(python3 -c 'import json; print(json.dumps([
  {"databaseId": 7, "headSha": "'"$(printf 'c%.0s' {1..40})"'", "status": "completed",
   "conclusion": "failure", "headBranch": "v0.1.2", "event": "push",
   "displayTitle": "IRIN Release", "name": "x"},
]))')"
printf '%s\n' "$FAIL_JSON" >"$GH_RUNS_FILE"
set +e
WAIT_OUT3="$(
  PATH="$FAKEBIN:$PATH" \
  IRIN_RELEASE_WORKFLOW_WAIT_ATTEMPTS=2 \
  IRIN_RELEASE_WORKFLOW_WAIT_SLEEP=0 \
  wait_for_tag_release_workflow "$TAG_WF" "$WANT_SHA" 2>&1
)"
WAIT_EC3=$?
set -e
[[ $WAIT_EC3 -ne 0 ]] || fail "failed workflow must refuse: $WAIT_OUT3"
[[ "$WAIT_OUT3" == *"failure"* || "$WAIT_OUT3" == *"concluded"* ]] \
  || fail "expected concluded-failure refuse: $WAIT_OUT3"
pass "real wait_for_tag_release_workflow + fake gh binds SHA/tag success"

# --- [P1] acceptance crash resume: acceptance without t2 completes t2 ------
# Build a full install-ready production candidate with pending-t2 + acceptance
# only (no t2), then resume (no tty needed for resume).
ACTION_ID="t2-resume-action"
rm -f "$DEST/proofs/t2.json"
printf '%s\n' '{
  "schema_version": 1,
  "packet_kind": "pending-t2",
  "action_id": "'"$ACTION_ID"'",
  "candidate_id": "'"$CID"'",
  "source_sha": "'"$SHA"'",
  "authorized_effects": ["tag-push", "release-attach", "publish", "version-image-labels"],
  "expiry": "2099-01-01T00:00:00Z"
}' >"$DEST/proofs/pending-t2.json"
seed_live_app_from "$DEST/IRIN.app"
write_proof "$DEST/proofs/acceptance.json" "acceptance" "$CID" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "dmg_sha256": "'"$DMG_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
  "pending_action_id": "'"$ACTION_ID"'",
  "installed_app_path": "'"$LIVE_APP"'",
}))')"
# Resume must not require tty
set +e
out="$("$ACCEPT" --candidate "$DEST" --installed-app "$LIVE_APP" 2>&1)"
ec=$?
set -e
[[ $ec -eq 0 ]] || fail "resume acceptance→t2 should succeed without tty: $out"
[[ -f "$DEST/proofs/t2.json" ]] || fail "resume must write t2.json"
# acceptance digest link
ACC_D="$(irin_sha256_file "$DEST/proofs/acceptance.json")"
python3 - "$DEST/proofs/t2.json" "$ACC_D" "$ACTION_ID" <<'PY' || fail "t2 does not link to acceptance"
import json, sys
d = json.load(open(sys.argv[1]))
assert d["acceptance_digest"] == sys.argv[2], d.get("acceptance_digest")
assert d["action_id"] == sys.argv[3]
assert d["result"] == "PASS"
print("t2 link ok")
PY
# Second resume should refuse (t2 exists)
set +e
out2="$("$ACCEPT" --candidate "$DEST" --installed-app "$LIVE_APP" 2>&1)"
ec2=$?
set -e
[[ $ec2 -ne 0 ]] || fail "second resume after t2 should refuse: $out2"
pass "acceptance→t2 crash resume completes matching T2 only"

# --- [P1] incomplete acceptance envelope refuses resume (no T2 mint) --------
rm -f "$DEST/proofs/t2.json"
printf '%s\n' '{
  "schema_version": 1,
  "packet_kind": "pending-t2",
  "action_id": "t2-incomplete-env",
  "candidate_id": "'"$CID"'",
  "source_sha": "'"$SHA"'",
  "authorized_effects": ["tag-push", "release-attach", "publish", "version-image-labels"],
  "expiry": "2099-01-01T00:00:00Z"
}' >"$DEST/proofs/pending-t2.json"
# Stripped envelope: missing tool_version/run_id/timestamp
python3 - "$DEST/proofs/acceptance.json" "$CID" "$SHA" "$DMG_D" "$BM_D" <<'PY'
import json, sys
path, cid, sha, dmg, bm = sys.argv[1:]
doc = {
  "schema_version": 1,
  "proof_kind": "acceptance",
  "candidate_id": cid,
  "source_sha": sha,
  "result": "PASS",
  # deliberately omit tool_version, run_id, timestamp
  "dmg_sha256": dmg,
  "installed_bundle_manifest_digest": bm,
  "pending_action_id": "t2-incomplete-env",
}
json.dump(doc, open(path, "w"), sort_keys=True, indent=2)
open(path, "a").write("\n")
PY
seed_live_app_from "$DEST/IRIN.app"
set +e
out_inc="$("$ACCEPT" --candidate "$DEST" --installed-app "$LIVE_APP" 2>&1)"
ec_inc=$?
set -e
[[ $ec_inc -ne 0 ]] || fail "incomplete acceptance envelope must refuse resume: $out_inc"
[[ "$out_inc" == *"tool_version"* || "$out_inc" == *"run_id"* || "$out_inc" == *"timestamp"* || "$out_inc" == *"mismatch"* ]] \
  || fail "expected envelope field refuse: $out_inc"
[[ ! -f "$DEST/proofs/t2.json" ]] || fail "incomplete resume must not write t2.json"
[[ -f "$DEST/proofs/pending-t2.json" ]] || fail "incomplete resume must not consume pending-t2"
pass "incomplete acceptance envelope refuses resume (pending preserved)"

# Fresh ACC_EXTRA must not interpolate paths into unquoted python.
grep -q 'os.environ\["INST_CANON"\]' "$ACCEPT" \
  || grep -q "os.environ\['INST_CANON'\]" "$ACCEPT" \
  || fail "acceptance extras must pass installed_app_path via env"
# Ensure no unquoted $INST_CANON inside a non-quoted heredoc payload builder.
if grep -n 'installed_app_path.*\$INST_CANON' "$ACCEPT" | grep -v 'os.environ'; then
  fail "installed_app_path must not be shell-interpolated into Python source"
fi
pass "fresh acceptance builds extras via env (no path interpolation)"

# --- [P1] link-ship-board never writes shared .irin-root -------------------
LINK="$ROOT/scripts/link-ship-board.sh"
grep -q '\.irin-root' "$LINK" && grep -E 'printf.*>.*\.irin-root|>"\$HOME_DIR/\.irin-root"' "$LINK" \
  && fail "link-ship-board must not write shared .irin-root" || true
# Explicit: no write of global pin
if grep -n 'HOME_DIR/.irin-root' "$LINK" | grep -v 'rm -f\|remove\|stale\|WARNING\|#'; then
  # only rm/warnings allowed
  if grep -n 'HOME_DIR/.irin-root' "$LINK" | grep -vE 'rm -f|stale|WARNING|#|test ! -f|\[\[ -f'; then
    fail "unexpected .irin-root write in link-ship-board"
  fi
fi
pass "link-ship-board does not write shared global .irin-root"

# --- public commands listed in Makefile help -------------------------------
help_out="$(make -C "$ROOT" help 2>/dev/null || true)"
for cmd in release-transaction install-verify candidate-status record-acceptance \
  link-ship-board shipping-method-smoke; do
  echo "$help_out" | grep -q "$cmd" || fail "Makefile help missing $cmd"
done
pass "Makefile help lists W3/W5 public commands"

# --- local tag peel: absent tag must not echo TAG^{commit} -----------------
# Hermetic publish always sets LOCAL_TAG_SHA=""; this is the non-hermetic first
# publish regression for the peel helper used by do_publish.
grep -q 'local_tag_peeled_or_empty' "$TX" \
  || fail "release-transaction must define local_tag_peeled_or_empty for publish"
grep -q 'rev-parse -q --verify' "$TX" \
  || fail "local tag peel must use rev-parse -q --verify (not bare || true echo)"
PEEL_REPO="$(mktemp -d "$TEST_HOME/peel-repo.XXXXXX")"
git -C "$PEEL_REPO" init -q
git -C "$PEEL_REPO" config user.email "w3@test.local"
git -C "$PEEL_REPO" config user.name "w3"
git -C "$PEEL_REPO" commit -q --allow-empty -m "peel-base"
# Source only the helper function (script is not designed as a library).
# shellcheck disable=SC1090
eval "$(
  sed -n '/^local_tag_peeled_or_empty()/,/^}/p' "$TX"
)"
(
  cd "$PEEL_REPO"
  # Buggy pattern still echoes the input on missing tags.
  bad="$(git rev-parse 'v0.0.0-absent^{commit}' 2>/dev/null || true)"
  [[ -n "$bad" ]] || fail "expected buggy rev-parse || true to leave a non-empty string"
  got="$(local_tag_peeled_or_empty 'v0.0.0-absent' || true)"
  [[ -z "$got" ]] || fail "absent local tag must peel empty (got $got)"
  HEAD_SHA="$(git rev-parse HEAD)"
  git tag -a 'v0.0.0-present' -m "t" HEAD
  got2="$(local_tag_peeled_or_empty 'v0.0.0-present')"
  [[ "$got2" == "$HEAD_SHA" ]] || fail "present tag peel mismatch: $got2 != $HEAD_SHA"
)
pass "local_tag_peeled_or_empty: absent empty, present peels SHA"

# --- live install (--live) staged swap + rollback + first-publish gate --------
# These contracts require macOS UDIF, mount, and ditto primitives. The Linux CI
# control-plane lane keeps all preceding W3 contracts and records this boundary.
if [[ "$(uname -s)" != "Darwin" ]]; then
  pass "macOS live install contracts skipped on non-Darwin"
  printf '\nAll W3 release-transaction contracts passed.\n'
  exit 0
fi

# Build a real UDIF DMG so install-verify can hdiutil-attach (no real /Applications).

make_live_candidate() {
  local label="$1"
  local stage src_dir dmg_path bm_d dmg_d app_d cid dest sha
  sha="$(sha40 "$label")"
  stage="$TEST_HOME/stage-live-$label"
  rm -rf "$stage"
  mkdir -p "$stage/IRIN.app/Contents/MacOS" "$stage/proofs" "$stage/smoke" "$stage/install" "$stage/logs"
  printf 'host-%s' "$label" >"$stage/IRIN.app/Contents/MacOS/council-warroom-tauri"
  printf 'side-%s' "$label" >"$stage/IRIN.app/Contents/MacOS/council"
  src_dir="$TEST_HOME/dmg-src-$label"
  rm -rf "$src_dir"
  mkdir -p "$src_dir"
  cp -R "$stage/IRIN.app" "$src_dir/IRIN.app"
  dmg_path="$stage/IRIN_0.1.2_aarch64.dmg"
  hdiutil create -volname "IRIN" -srcfolder "$src_dir" -ov -format UDZO "$dmg_path" >/dev/null \
    || fail "hdiutil create failed for live candidate $label"
  irin_write_bundle_manifest "$stage/IRIN.app" "$stage/bundle-manifest.txt"
  bm_d="$(irin_sha256_file "$stage/bundle-manifest.txt")"
  dmg_d="$(irin_sha256_file "$dmg_path")"
  app_d="$(irin_sha256_file "$stage/IRIN.app/Contents/MacOS/council-warroom-tauri")"
  cat >"$stage/HASHES.txt" <<EOF
pack_mode=production
release_version=0.1.2
releasable=true
stapled=true
source_sha=$sha
build_dirty=false
arch=aarch64-apple-darwin
app=IRIN.app
dmg=IRIN_0.1.2_aarch64.dmg
app_sha256=$app_d
council_sha256=$(irin_sha256_file "$stage/IRIN.app/Contents/MacOS/council")
arm_attest_sha256=$(printf 'x' | irin_sha256_bytes)
gateway_pack_compose_sha256=$(printf 'y' | irin_sha256_bytes)
gateway_pack_manifest_sha256=$(printf 'z' | irin_sha256_bytes)
gateway_digest=$(python3 -c 'print("g"+"0"*63)')
sidecar_digest=$(python3 -c 'print("s"+"0"*63)')
warroom_web_index_sha256=$(printf 'w' | irin_sha256_bytes)
bundle_manifest_digest=$bm_d
dmg_sha256=$dmg_d
EOF
  python3 - "$stage/candidate.json" "$sha" "$bm_d" "$dmg_d" <<'PY2'
import json, sys
out, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.1.2",
  "pack_mode": "production",
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": True,
  "gateway_digest": "g" + ("0" * 63),
  "sidecar_digest": "s" + ("0" * 63),
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY2
  cid="$(irin_sha256_file "$stage/candidate.json")"
  dest="$IRIN_CANDIDATE_ROOT/0.1.2/$sha/$cid"
  irin_promote_candidate_from_staging "$stage" "$dest" >/dev/null
  chmod -R u+w "$dest/proofs" "$dest/install" 2>/dev/null || true
  write_proof "$dest/proofs/verify.json" "verify" "$cid" "$sha" "PASS" "$(python3 -c 'import json; print(json.dumps({
    "dmg_sha256": "'"$dmg_d"'",
    "bundle_manifest_digest": "'"$bm_d"'",
  }))')"
  printf '%s\n' "$dest"
}

# (a) --live staged swap success + exact digest at hermetic Applications
LIVE_DEST="$(make_live_candidate ok)"
LIVE_CID="$(basename "$LIVE_DEST")"
LIVE_BM="$(irin_sha256_file "$LIVE_DEST/bundle-manifest.txt")"
LIVE_SHA="$(python3 -c 'import json; print(json.load(open("'"$LIVE_DEST"'/candidate.json"))["source_sha"])')"
# Prior daily-use app (different bytes) must be displaced and archived.
rm -rf "$LIVE_APP"
mkdir -p "$LIVE_APP/Contents/MacOS"
printf 'old-daily' >"$LIVE_APP/Contents/MacOS/council-warroom-tauri"
printf 'old-side' >"$LIVE_APP/Contents/MacOS/council"
set +e
LIVE_OUT="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  IRIN_STATE_ROOT="$IRIN_STATE_ROOT" \
  "$INSTALL" --candidate "$LIVE_DEST" --live 2>&1
)"
LIVE_EC=$?
set -e
[[ $LIVE_EC -eq 0 ]] || fail "--live install should succeed: $LIVE_OUT"
[[ -d "$LIVE_APP" ]] || fail "--live must leave app at hermetic Applications"
[[ -f "$LIVE_DEST/proofs/install.json" ]] || fail "--live success must write install proof"
python3 - "$LIVE_DEST/proofs/install.json" "$LIVE_APP" "$LIVE_BM" <<'PY2' || fail "install proof live fields wrong"
import json, sys, os
d = json.load(open(sys.argv[1]))
live_path, want_digest = sys.argv[2], sys.argv[3]
assert d.get("result") == "PASS"
assert d.get("live_installed_bundle_manifest_digest") == want_digest, d.get("live_installed_bundle_manifest_digest")
assert os.path.samefile(d["live_installed_app_path"], live_path), (d.get("live_installed_app_path"), live_path)
assert d.get("installed_bundle_manifest_digest") == want_digest
print("live fields ok")
PY2
TMP_LIVE_BM="$(mktemp)"
irin_write_bundle_manifest "$LIVE_APP" "$TMP_LIVE_BM"
GOT_LIVE="$(irin_sha256_file "$TMP_LIVE_BM")"
rm -f "$TMP_LIVE_BM"
[[ "$GOT_LIVE" == "$LIVE_BM" ]] || fail "live app digest $GOT_LIVE != candidate $LIVE_BM"
# Prior app archived under state root, not left as sibling clutter in Applications.
[[ -z "$(find "$IRIN_LIVE_APPLICATIONS_ROOT" -maxdepth 1 -name 'IRIN.app.irin-*' 2>/dev/null)" ]] \
  || fail "staging/prior siblings must not remain under Applications"
ARCH_COUNT="$(find "$IRIN_STATE_ROOT/displaced-apps" -maxdepth 1 -type d -name 'IRIN.app.*' 2>/dev/null | wc -l | tr -d ' ')"
[[ "$ARCH_COUNT" -ge 1 ]] || fail "displaced prior app must be archived under state root"
pass "--live staged swap success + exact digest at hermetic Applications"

# Static: startup must never rm PRIOR; stale PID-scoped prior hard-refuses.
grep -q 'stale PID-scoped prior exists' "$INSTALL" \
  || fail "install-verify must hard-refuse existing PID-scoped PRIOR"
if grep -nE 'rm -rf[[:space:]]+"?\$PRIOR"?' "$INSTALL" | grep -v '^[[:space:]]*#'; then
  fail "install-verify must never rm PRIOR (stale prior is recovery data)"
fi
grep -q 'SAVED_PRIOR equals LIVE_APP' "$INSTALL" \
  || fail "live_rollback must refuse SAVED_PRIOR == LIVE_APP nesting"
pass "static: stale PRIOR refuse + never rm PRIOR + no nested restore"

# (b) post-archive / proof-write failure: make proofs/ unwritable so durable
# install.json cannot land after swap+archive; prior must restore; saved source kept.
LIVE_DEST_B="$(make_live_candidate rb)"
rm -rf "$LIVE_APP"
mkdir -p "$LIVE_APP/Contents/MacOS"
printf 'prior-keep' >"$LIVE_APP/Contents/MacOS/council-warroom-tauri"
printf 'prior-side' >"$LIVE_APP/Contents/MacOS/council"
PRIOR_MARKER="$(cat "$LIVE_APP/Contents/MacOS/council-warroom-tauri")"
rm -f "$LIVE_DEST_B/proofs/install.json"
# Ensure proofs dir exists then freeze it unwritable (natural late failure).
mkdir -p "$LIVE_DEST_B/proofs"
chmod -R u+w "$LIVE_DEST_B/proofs" 2>/dev/null || true
chmod a-w "$LIVE_DEST_B/proofs" || fail "could not make proofs unwritable"
set +e
LIVE_OUT_B="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  IRIN_STATE_ROOT="$IRIN_STATE_ROOT" \
  "$INSTALL" --candidate "$LIVE_DEST_B" --live 2>&1
)"
LIVE_EC_B=$?
set -e
# Always restore perms for cleanup even on assertion fail.
chmod -R u+w "$LIVE_DEST_B/proofs" 2>/dev/null || true
[[ $LIVE_EC_B -ne 0 ]] || fail "unwritable proofs after swap must refuse: $LIVE_OUT_B"
[[ ! -f "$LIVE_DEST_B/proofs/install.json" ]] \
  || fail "post-swap proof failure must leave no install proof"
[[ -d "$LIVE_APP" ]] || fail "rollback must restore prior live app"
[[ "$(cat "$LIVE_APP/Contents/MacOS/council-warroom-tauri")" == "$PRIOR_MARKER" ]] \
  || fail "rollback must restore prior app bytes"
# Saved prior must not be deleted if still present under displaced-apps or restored.
[[ -z "$(find "$IRIN_LIVE_APPLICATIONS_ROOT" -maxdepth 1 -name 'IRIN.app.irin-prior.*' 2>/dev/null)" ]] \
  || fail "unrestored prior sibling must not remain after successful restore"
pass "--live rollback restores prior app and writes no install proof"

# Default (no --live): reuse existing real candidate; must not touch Applications.
rm -rf "$LIVE_APP"
mkdir -p "$LIVE_APP/Contents/MacOS"
printf 'untouched' >"$LIVE_APP/Contents/MacOS/council-warroom-tauri"
rm -f "$LIVE_DEST/proofs/install.json"
set +e
DEF_OUT="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  "$INSTALL" --candidate "$LIVE_DEST" 2>&1
)"
DEF_EC=$?
set -e
[[ $DEF_EC -eq 0 ]] || fail "default install-verify should succeed: $DEF_OUT"
[[ "$(cat "$LIVE_APP/Contents/MacOS/council-warroom-tauri")" == "untouched" ]] \
  || fail "default mode must not mutate live Applications"
[[ -f "$LIVE_DEST/proofs/install.json" ]] || fail "default mode must write install proof"
python3 - "$LIVE_DEST/proofs/install.json" <<'PY2' || fail "default proof must lack live_* fields"
import json, sys
d = json.load(open(sys.argv[1]))
assert "live_installed_app_path" not in d
assert "live_installed_bundle_manifest_digest" not in d
print("no live fields")
PY2
pass "default install-verify unchanged (no live fields, no Applications write)"

# (d) first-publish live digest mismatch refuses (helper; no real publish I/O)
(
  set -euo pipefail
  IRIN_RELEASE_TX_LIB=1
  # shellcheck source=/dev/null
  source "$TX"
  export IRIN_CANDIDATE_STATUS_HERMETIC=1
  export IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT"
  export IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT"
  rm -rf "$LIVE_APP"
  mkdir -p "$LIVE_APP/Contents/MacOS"
  printf 'mismatch' >"$LIVE_APP/Contents/MacOS/council-warroom-tauri"
  printf 'mismatch' >"$LIVE_APP/Contents/MacOS/council"
  set +e
  out="$(require_live_app_matches_candidate "$LIVE_DEST" 2>&1)"
  ec=$?
  set -e
  [[ $ec -ne 0 ]] || { echo "expected live mismatch refuse: $out" >&2; exit 1; }
  [[ "$out" == *"mismatch"* || "$out" == *"digest"* ]] || { echo "bad msg: $out" >&2; exit 1; }
)
pass "first publish refuses live digest mismatch"

# Matching live app passes the gate helper.
seed_live_app_from "$LIVE_DEST/IRIN.app"
(
  set -euo pipefail
  IRIN_RELEASE_TX_LIB=1
  # shellcheck source=/dev/null
  source "$TX"
  export IRIN_CANDIDATE_STATUS_HERMETIC=1
  export IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT"
  export IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT"
  require_live_app_matches_candidate "$LIVE_DEST"
)
pass "first-publish live gate accepts matching live app"

# (e) already-Published skip + malformed publication refuse (same helper do_publish uses)
grep -q 'maybe_require_first_publish_live_app' "$TX" \
  || fail "do_publish must call maybe_require_first_publish_live_app"
grep -q 'publication proof reaches Published: skip first-publish live app gate' "$TX" \
  || fail "publish must skip live gate only when publication proof reaches Published"
grep -q 'hermetic: skip first-publish live app gate' "$TX" \
  || fail "publish must skip live gate under publish_hermetic_active"
grep -q 'symlink-root live app' "$TX" \
  || fail "first-publish live gate must refuse symlink-root live app"
grep -q 'must not be a symlink at bundle root' "$ACCEPT" \
  || fail "T2 acceptance must refuse symlink-root installed app"
grep -q 'pwd -P' "$INSTALL" \
  || fail "live install must physically resolve displaced-apps containment"
python3 - "$TX" <<'PY' || fail "do_publish must invoke maybe_require_first_publish_live_app before mutation"
import sys
text = open(sys.argv[1]).read()
pub = text.split("do_publish()")[1]
i_helper = pub.find("maybe_require_first_publish_live_app")
i_tag = pub.find("check remote tag peeled")
assert i_helper >= 0, "helper call missing in do_publish"
assert i_tag < 0 or i_helper < i_tag, "live gate must precede remote tag mutation checks"
print("do_publish already-Published branch order ok")
PY

# Symlink-root live app refuses first-publish gate (before hashing).
rm -rf "$LIVE_APP"
mkdir -p "$IRIN_LIVE_APPLICATIONS_ROOT/real-extract/Contents/MacOS"
printf 'linked' >"$IRIN_LIVE_APPLICATIONS_ROOT/real-extract/Contents/MacOS/council-warroom-tauri"
printf 'linked' >"$IRIN_LIVE_APPLICATIONS_ROOT/real-extract/Contents/MacOS/council"
ln -s "$IRIN_LIVE_APPLICATIONS_ROOT/real-extract" "$LIVE_APP"
(
  set -euo pipefail
  IRIN_RELEASE_TX_LIB=1
  # shellcheck source=/dev/null
  source "$TX"
  export IRIN_CANDIDATE_STATUS_HERMETIC=1
  export IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT"
  export IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT"
  set +e
  out="$(require_live_app_matches_candidate "$LIVE_DEST" 2>&1)"
  ec=$?
  set -e
  [[ $ec -ne 0 ]] || { echo "expected symlink-root refuse: $out" >&2; exit 1; }
  [[ "$out" == *"symlink"* ]] || { echo "bad msg: $out" >&2; exit 1; }
)
pass "first-publish refuses symlink-root live app"

# Symlink-root installed app refuses T2 pin.
set +e
out_sym="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT" \
  "$ACCEPT" --candidate "$LIVE_DEST" --installed-app "$LIVE_APP" 2>&1
)"
ec_sym=$?
set -e
[[ $ec_sym -ne 0 ]] || fail "T2 must refuse symlink-root installed app: $out_sym"
[[ "$out_sym" == *"symlink"* ]] \
  || fail "expected symlink refuse for T2: $out_sym"
pass "T2 acceptance refuses symlink-root installed app"
rm -rf "$LIVE_APP" "$IRIN_LIVE_APPLICATIONS_ROOT/real-extract"

# Hex-valid source_sha fixture (label "a0" → a0a0… is 40-char lowercase hex).
# Non-hex labels like "ok" never become well_formed under candidate-status.
PUB_DEST="$(make_live_candidate a0)"
PUB_CID="$(basename "$PUB_DEST")"
PUB_SHA="$(python3 -c 'import json; print(json.load(open("'"$PUB_DEST"'/candidate.json"))["source_sha"])')"
PUB_BM_D="$(python3 -c 'import json; print(json.load(open("'"$PUB_DEST"'/candidate.json"))["bundle_manifest_digest"])')"
PUB_DMG_D="$(python3 -c 'import json; print(json.load(open("'"$PUB_DEST"'/candidate.json"))["dmg_sha256"])')"
mkdir -p "$PUB_DEST/install"
cp -R "$PUB_DEST/IRIN.app" "$PUB_DEST/install/IRIN.app"
chmod -R u+w "$PUB_DEST/install" 2>/dev/null || true
irin_write_bundle_manifest "$PUB_DEST/install/IRIN.app" "$PUB_DEST/install/bundle-manifest.txt"
write_proof "$PUB_DEST/proofs/install.json" "install" "$PUB_CID" "$PUB_SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "candidate_bundle_manifest_digest": "'"$PUB_BM_D"'",
  "installed_bundle_manifest_digest": "'"$PUB_BM_D"'",
}))')"
PUB_ACTION="t2-w3-published"
write_proof "$PUB_DEST/proofs/acceptance.json" "acceptance" "$PUB_CID" "$PUB_SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "dmg_sha256": "'"$PUB_DMG_D"'",
  "installed_bundle_manifest_digest": "'"$PUB_BM_D"'",
  "pending_action_id": "'"$PUB_ACTION"'",
  "installed_app_path": "'"$PUB_DEST"'/install/IRIN.app",
}))')"
PUB_ACC_D="$(irin_sha256_file "$PUB_DEST/proofs/acceptance.json")"
write_proof "$PUB_DEST/proofs/t2.json" "t2" "$PUB_CID" "$PUB_SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "action_id": "'"$PUB_ACTION"'",
  "acceptance_digest": "'"$PUB_ACC_D"'",
  "authorized_effects": ["tag-push", "release-attach", "publish", "version-image-labels"],
  "expiry": "2099-01-01T00:00:00Z",
}))')"
tier_acc="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$STATUS" --candidate "$PUB_DEST" --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tier") or "")'
)"
[[ "$tier_acc" == "Accepted" ]] || fail "Published-skip fixture must first reach Accepted (got '$tier_acc')"

# Malformed publication proof (file present) must refuse skip — not idempotent.
write_proof "$PUB_DEST/proofs/publication.json" "publication" "$PUB_CID" "$PUB_SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "public_state": "draft",
  "redownload_unauthenticated": False,
  "asset_sha256": "0" * 64,
  "tag": "v0.1.2",
}))')"
seed_live_app_from "$PUB_DEST/IRIN.app"
(
  set -euo pipefail
  IRIN_RELEASE_TX_LIB=1
  # shellcheck source=/dev/null
  source "$TX"
  export IRIN_CANDIDATE_STATUS_HERMETIC=1
  export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
  export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true
  export IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT"
  export IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT"
  # Ensure not hermetic-publish (would skip without Published).
  unset IRIN_PUBLISH_HERMETIC IRIN_PUBLISH_HERMETIC_CONFIRM || true
  set +e
  out="$(maybe_require_first_publish_live_app "$PUB_DEST" 2>&1)"
  ec=$?
  set -e
  [[ $ec -ne 0 ]] || { echo "expected malformed publication refuse: $out" >&2; exit 1; }
  [[ "$out" == *"Published"* || "$out" == *"refuse"* ]] \
    || { echo "bad msg: $out" >&2; exit 1; }
)
pass "malformed publication proof refuses first-publish live-gate skip"

# Valid Published proof: real helper skips even when live app is corrupted.
write_proof "$PUB_DEST/proofs/publication.json" "publication" "$PUB_CID" "$PUB_SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "public_state": "published",
  "redownload_unauthenticated": True,
  "asset_sha256": "'"$PUB_DMG_D"'",
  "dmg_sha256": "'"$PUB_DMG_D"'",
  "tag": "v0.1.2",
  "repo": "irinityhq/irin",
  "release_url": "https://example.test/releases/tag/v0.1.2",
}))')"
tier_pub="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$STATUS" --candidate "$PUB_DEST" --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tier") or "")'
)"
[[ "$tier_pub" == "Published" ]] || fail "fixture must reach Published (got '$tier_pub')"
rm -rf "$LIVE_APP"
mkdir -p "$LIVE_APP/Contents/MacOS"
printf 'later-install' >"$LIVE_APP/Contents/MacOS/council-warroom-tauri"
(
  set -euo pipefail
  IRIN_RELEASE_TX_LIB=1
  # shellcheck source=/dev/null
  source "$TX"
  export IRIN_CANDIDATE_STATUS_HERMETIC=1
  export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
  export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true
  export IRIN_LIVE_APPLICATIONS_ROOT="$IRIN_LIVE_APPLICATIONS_ROOT"
  export IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT"
  unset IRIN_PUBLISH_HERMETIC IRIN_PUBLISH_HERMETIC_CONFIRM || true
  out="$(maybe_require_first_publish_live_app "$PUB_DEST" 2>&1)"
  [[ "$out" == *"publication proof reaches Published"* ]] \
    || { echo "expected Published skip: $out" >&2; exit 1; }
)
pass "real maybe_require_first_publish_live_app skips when already Published"

# Containment of IRIN_LIVE_APPLICATIONS_ROOT is covered by the production pin
# test above (non-hermetic + override + temp live path still requires /Applications).
grep -q 'hermetic_overrides_allowed' "$INSTALL" \
  || fail "install-verify must reuse hermetic_overrides_allowed containment"
grep -q 'IRIN_LIVE_APPLICATIONS_ROOT' "$INSTALL" \
  || fail "install-verify must document IRIN_LIVE_APPLICATIONS_ROOT override"

# (f) hermetic publish dual-gate still present (full rehearsal via shipping-method-smoke)
grep -q 'publish_hermetic_active' "$TX" || fail "publish_hermetic_active missing"
pass "hermetic publish path remains present for shipping-method-smoke"

printf '\nAll W3 release-transaction contracts passed.\n'
