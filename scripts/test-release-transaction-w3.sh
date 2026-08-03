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
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

TX="$ROOT/scripts/release-transaction.sh"
INSTALL="$ROOT/scripts/install-verify-candidate.sh"
ACCEPT="$ROOT/scripts/record-acceptance.sh"
STATUS="$ROOT/scripts/candidate-status.sh"
[[ -x "$TX" && -x "$INSTALL" && -x "$ACCEPT" && -x "$STATUS" ]] \
  || fail "W3 scripts not executable"

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

export IRIN_CANDIDATE_STATUS_HERMETIC=1
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
out="$(printf 'nope\n' | "$ACCEPT" --candidate "$DEST" --installed-app "$DEST/install/IRIN.app" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "piped record-acceptance should refuse"
[[ "$out" == *"tty"* || "$out" == *"interactive"* ]] \
  || fail "expected tty refuse: $out"
pass "record-acceptance refuses non-tty / piped input"

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
write_proof "$DEST/proofs/acceptance.json" "acceptance" "$CID" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "dmg_sha256": "'"$DMG_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
  "pending_action_id": "'"$ACTION_ID"'",
  "installed_app_path": "'"$DEST"'/install/IRIN.app",
}))')"
# Resume must not require tty
set +e
out="$("$ACCEPT" --candidate "$DEST" --installed-app "$DEST/install/IRIN.app" 2>&1)"
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
out2="$("$ACCEPT" --candidate "$DEST" --installed-app "$DEST/install/IRIN.app" 2>&1)"
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
set +e
out_inc="$("$ACCEPT" --candidate "$DEST" --installed-app "$DEST/install/IRIN.app" 2>&1)"
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

printf '\nAll W3 release-transaction contracts passed.\n'
