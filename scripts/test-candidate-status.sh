#!/usr/bin/env bash
# Hermetic contracts for scripts/candidate-status.sh (W2).
# Zero network, zero Apple, zero provider. Pins the adapter JSON schema and
# refuses forged/schema-invalid evidence.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Keep fixture store off the monorepo (env.sh refuses worktree roots).
TEST_HOME="$(mktemp -d "/tmp/irin-cand-status-test.XXXXXX")"
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

STATUS="$ROOT/scripts/candidate-status.sh"
FIXTURES="$ROOT/scripts/fixtures/candidate-status"
[[ -x "$STATUS" ]] || fail "candidate-status.sh not executable"

# Hermetic external facts (never hit network/gh in this suite).
# Overrides only apply with HERMETIC=1 and a temp-store root (both set here).
export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true

sha40() { python3 -c "print(('$1' * 40)[:40])"; }

make_staging() {
  local staging="$1" pack_mode="$2" source_sha="$3" dmg_body="$4"
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
releasable=false
stapled=false
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
  python3 - "$staging/candidate.json" "$pack_mode" "$source_sha" "$bm_d" "$dmg_d" <<'PY'
import json, sys
out, pack_mode, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.1.2",
  "pack_mode": pack_mode,
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": False,
  "gateway_digest": "g" + ("0" * 63),
  "sidecar_digest": "s" + ("0" * 63),
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
}

write_proof() {
  # write_proof PATH KIND CID SHA RESULT [extra_json_object]
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
    if k in doc:
        raise SystemExit(f"extra collides: {k}")
    doc[k] = v
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY
}

status_json() {
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
    IRIN_CANDIDATE_STATUS_HERMETIC="${IRIN_CANDIDATE_STATUS_HERMETIC:-}" \
    IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN="${IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN}" \
    IRIN_CANDIDATE_STATUS_CI_REQUIRED="${IRIN_CANDIDATE_STATUS_CI_REQUIRED}" \
    "$STATUS" --candidate "$1" --json
}

# Stage a complete install/ tree (app + recomputable bundle-manifest).
stage_install_tree() {
  local dest="$1"
  rm -rf "$dest/install"
  mkdir -p "$dest/install"
  # Copy the stored app bytes into install/ (simulates DMG extract).
  cp -R "$dest/IRIN.app" "$dest/install/IRIN.app"
  # proofs/ remains writable; install payload may be frozen on real path.
  chmod -R u+w "$dest/install" 2>/dev/null || true
  irin_write_bundle_manifest "$dest/install/IRIN.app" "$dest/install/bundle-manifest.txt"
}

assert_tier() {
  local path="$1" expected="$2"
  local got
  got="$(status_json "$path" | python3 -c 'import json,sys; t=json.load(sys.stdin).get("tier"); print(t if t is not None else "")')"
  [[ "$got" == "$expected" ]] || fail "expected tier '$expected', got '$got' for $path"
}

assert_field() {
  local path="$1" expr="$2" expected="$3"
  local got
  got="$(status_json "$path" | python3 -c "import json,sys; d=json.load(sys.stdin); print($expr)")"
  [[ "$got" == "$expected" ]] || fail "expected $expr == '$expected', got '$got'"
}

validate_schema_shape() {
  local json_file="$1"
  python3 - "$json_file" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
required = {
  "schema_version", "reporter", "candidate_path", "candidate_id", "source_sha",
  "semver", "pack_mode", "dmg_sha256", "bundle_manifest_digest", "well_formed",
  "tier", "blockers", "checks", "caveats",
}
missing = sorted(required - set(d))
assert not missing, f"missing keys: {missing}"
assert d["schema_version"] == 1
assert d["reporter"] == "scripts/candidate-status.sh"
assert isinstance(d["blockers"], list)
assert isinstance(d["caveats"], list)
assert isinstance(d["checks"], dict)
for k in (
  "identity_ok", "payload_ok", "source_on_main", "ci_required_green",
  "verify_proof", "install_proof", "acceptance_proof", "publication_proof",
):
  assert k in d["checks"], f"checks missing {k}"
for b in d["blockers"]:
  assert set(b) >= {"code", "message", "blocks_tier"}
print("schema shape ok")
PY
}

# --- promote a base candidate ----------------------------------------------
SHA="$(sha40 a)"
S1="$TEST_HOME/stage1"
make_staging "$S1" "local-dev" "$SHA" "dmg-bytes-A"
CID="$(irin_sha256_file "$S1/candidate.json")"
DEST="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA/$CID"
irin_promote_candidate_from_staging "$S1" "$DEST" >/dev/null
# proofs stay writable after freeze
chmod u+w "$DEST/proofs" 2>/dev/null || true

# --- refuse path outside store ---------------------------------------------
set +e
out="$("$STATUS" --candidate /tmp --json 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "path outside store should refuse"
[[ "$out" == *"IRIN_CANDIDATE_ROOT"* ]] || fail "expected root refuse: $out"
pass "refuses candidate path outside IRIN_CANDIDATE_ROOT"

# --- relative path refused -------------------------------------------------
set +e
out="$("$STATUS" --candidate relative/path 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "relative path should refuse"
pass "refuses relative --candidate path"

# --- well-formed, no verify → tier none ------------------------------------
tmp_status="$TEST_HOME/status1.json"
status_json "$DEST" >"$tmp_status"
validate_schema_shape "$tmp_status"
assert_tier "$DEST" ""
assert_field "$DEST" 'd["well_formed"]' "True"
assert_field "$DEST" 'd["checks"]["identity_ok"]' "True"
assert_field "$DEST" 'd["checks"]["verify_proof"]' "False"
pass "well-formed bare candidate is below Candidate verified"

# --- --require fails below tier --------------------------------------------
set +e
"$STATUS" --candidate "$DEST" --require "Candidate verified" >/dev/null 2>&1
ec=$?
set -e
[[ $ec -eq 1 ]] || fail "--require below tier should exit 1 (got $ec)"
pass "--require exits 1 when below requested tier"

# --- identical bytes → identical tier (repeat run) -------------------------
r1="$(status_json "$DEST")"
r2="$(status_json "$DEST")"
[[ "$r1" == "$r2" ]] || fail "repeat runs on identical bytes must match"
pass "identical tier on repeat runs from identical bytes"

# --- verify PASS + external green → Candidate verified ---------------------
BM_D="$(python3 -c 'import json; print(json.load(open("'"$DEST"'/candidate.json"))["bundle_manifest_digest"])')"
DMG_D="$(python3 -c 'import json; print(json.load(open("'"$DEST"'/candidate.json"))["dmg_sha256"])')"
EXTRA="$(python3 -c 'import json; print(json.dumps({"dmg_sha256":"'"$DMG_D"'","bundle_manifest_digest":"'"$BM_D"'"}))')"
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "PASS" "$EXTRA"
assert_tier "$DEST" "Candidate verified"
assert_field "$DEST" 'd["checks"]["verify_proof"]' "True"
status_json "$DEST" >"$TEST_HOME/verified.json"
validate_schema_shape "$TEST_HOME/verified.json"
pass "verify.json PASS + main/CI green → Candidate verified"

# --- --require met exits 0 -------------------------------------------------
"$STATUS" --candidate "$DEST" --require "Candidate verified" >/dev/null
pass "--require Candidate verified exits 0 when met"

# --- forged PASS + wrong hash does not advance -----------------------------
# Mutate verify proof dmg hash while keeping result=PASS.
python3 - "$DEST/proofs/verify.json" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["dmg_sha256"] = "f" * 64
d["result"] = "PASS"
json.dump(d, open(p, "w"), sort_keys=True, indent=2)
open(p, "a").write("\n")
PY
assert_tier "$DEST" ""
assert_field "$DEST" 'd["checks"]["verify_proof"]' "False"
# restore good verify
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "PASS" "$EXTRA"
assert_tier "$DEST" "Candidate verified"
pass "forged PASS with wrong hash does not advance tier"

# --- schema-invalid / result!=PASS does not advance ------------------------
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "FAIL" "$EXTRA"
assert_tier "$DEST" ""
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "PASS" "$EXTRA"
assert_tier "$DEST" "Candidate verified"
pass "result=FAIL does not yield Candidate verified"

# --- network unavailable never greens --------------------------------------
IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=unavailable \
IRIN_CANDIDATE_STATUS_CI_REQUIRED=unavailable \
  assert_tier "$DEST" ""
# restore
export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true
assert_tier "$DEST" "Candidate verified"
pass "unavailable external facts leave candidate below Candidate verified"

# --- [P1] app mutation invalidates Candidate verified ----------------------
chmod -R u+w "$DEST/IRIN.app" 2>/dev/null || true
printf 'MUTATED-COUNCIL' >"$DEST/IRIN.app/Contents/MacOS/council"
assert_tier "$DEST" ""
assert_field "$DEST" 'd["checks"]["payload_ok"]' "False"
# Restore content + freeze-normalized mode from the stored bundle-manifest.
printf 'side' >"$DEST/IRIN.app/Contents/MacOS/council"
stored_council_mode="$(awk -F'\t' '$1=="Contents/MacOS/council"{print $3; exit}' "$DEST/bundle-manifest.txt")"
# Current mode may keep write bits; freeze-norm equality only needs r/x match.
# Set freeze-norm of stored (clear write bits) so a-w and pre-freeze both ok.
norm_mode="$(python3 -c "print(format(int('$stored_council_mode',8)&~0o222,'04o'))")"
chmod "$norm_mode" "$DEST/IRIN.app/Contents/MacOS/council"
assert_tier "$DEST" "Candidate verified"
assert_field "$DEST" 'd["checks"]["payload_ok"]' "True"
pass "app mutation recomputes bundle-manifest and drops Candidate verified"

# --- [P1] executable-bit loss invalidates Candidate verified ---------------
# Ensure stored manifest has 0755 for council, then drop to 0644 (not merely a-w).
# Rebuild a candidate whose manifest records executable council.
S_EXEC="$TEST_HOME/stage-exec"
make_staging "$S_EXEC" "local-dev" "$SHA" "dmg-bytes-exec"
chmod 0755 "$S_EXEC/IRIN.app/Contents/MacOS/council"
# Rewrite manifest + identity after mode change so stored mode is 0755.
irin_write_bundle_manifest "$S_EXEC/IRIN.app" "$S_EXEC/bundle-manifest.txt"
BM_EXEC="$(irin_sha256_file "$S_EXEC/bundle-manifest.txt")"
DMG_EXEC="$(irin_sha256_file "$S_EXEC/IRIN_0.1.2_aarch64.dmg")"
python3 - "$S_EXEC/candidate.json" "$SHA" "$BM_EXEC" "$DMG_EXEC" <<'PY'
import json, sys
out, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.1.2",
  "pack_mode": "local-dev",
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": False,
  "gateway_digest": "g" + ("0" * 63),
  "sidecar_digest": "s" + ("0" * 63),
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
# Refresh HASHES digests that must match identity.
python3 - "$S_EXEC/HASHES.txt" "$BM_EXEC" "$DMG_EXEC" <<'PY'
import sys
path, bm, dmg = sys.argv[1:]
lines = open(path).read().splitlines()
out = []
for line in lines:
    if line.startswith("bundle_manifest_digest="):
        out.append(f"bundle_manifest_digest={bm}")
    elif line.startswith("dmg_sha256="):
        out.append(f"dmg_sha256={dmg}")
    else:
        out.append(line)
open(path, "w").write("\n".join(out) + "\n")
PY
CID_EXEC="$(irin_sha256_file "$S_EXEC/candidate.json")"
DEST_EXEC="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA/$CID_EXEC"
irin_promote_candidate_from_staging "$S_EXEC" "$DEST_EXEC" >/dev/null
EXTRA_EXEC="$(python3 -c 'import json; print(json.dumps({"dmg_sha256":"'"$DMG_EXEC"'","bundle_manifest_digest":"'"$BM_EXEC"'"}))')"
write_proof "$DEST_EXEC/proofs/verify.json" "verify" "$CID_EXEC" "$SHA" "PASS" "$EXTRA_EXEC"
assert_tier "$DEST_EXEC" "Candidate verified"
# Pure freeze a-w leaves executable bits (0755→0555) — still Verified.
stored_mode="$(awk -F'\t' '$1=="Contents/MacOS/council"{print $3; exit}' "$DEST_EXEC/bundle-manifest.txt")"
[[ "$stored_mode" == "0755" ]] || fail "expected stored council mode 0755, got $stored_mode"
# Portable mode bits: GNU stat -c first. On Linux, stat -f is --file-system
# (succeeds with a multi-line dump), so BSD-first order falsely "succeeds".
frozen_mode="$(stat -c '%a' "$DEST_EXEC/IRIN.app/Contents/MacOS/council" 2>/dev/null \
  || stat -f '%Lp' "$DEST_EXEC/IRIN.app/Contents/MacOS/council")"
[[ "$frozen_mode" == "555" || "$frozen_mode" == "0555" ]] \
  || fail "expected frozen council mode 0555, got $frozen_mode"
assert_tier "$DEST_EXEC" "Candidate verified"
# Drop execute (0644 freeze-norms to 0444 ≠ 0555) — must leave Verified.
chmod u+w "$DEST_EXEC/IRIN.app/Contents/MacOS/council" 2>/dev/null || true
chmod 0644 "$DEST_EXEC/IRIN.app/Contents/MacOS/council"
assert_tier "$DEST_EXEC" ""
assert_field "$DEST_EXEC" 'd["checks"]["payload_ok"]' "False"
# Restore freeze-norm equivalent of 0755 → 0555
chmod 0555 "$DEST_EXEC/IRIN.app/Contents/MacOS/council"
assert_tier "$DEST_EXEC" "Candidate verified"
pass "executable-bit loss (0755→0644) drops Candidate verified; freeze 0555 ok"

# --- [P1] verify without required hash bindings refuses --------------------
python3 - "$DEST/proofs/verify.json" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d.pop("dmg_sha256", None)
d.pop("bundle_manifest_digest", None)
json.dump(d, open(p, "w"), sort_keys=True, indent=2)
open(p, "a").write("\n")
PY
assert_tier "$DEST" ""
write_proof "$DEST/proofs/verify.json" "verify" "$CID" "$SHA" "PASS" "$EXTRA"
assert_tier "$DEST" "Candidate verified"
pass "verify.json requires dmg_sha256 + bundle_manifest_digest bindings"

# --- [P1] empty install/ + forged install.json does not yield Installed ----
INST_EXTRA="$(python3 -c 'import json; print(json.dumps({
  "candidate_bundle_manifest_digest": "'"$BM_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
}))')"
rm -rf "$DEST/install"
mkdir -p "$DEST/install"
write_proof "$DEST/proofs/install.json" "install" "$CID" "$SHA" "PASS" "$INST_EXTRA"
assert_tier "$DEST" "Candidate verified"
assert_field "$DEST" 'd["checks"]["install_proof"]' "False"
pass "empty install/ with forged install.json does not yield Installed"

# --- install proof + real install tree → Installed -------------------------
stage_install_tree "$DEST"
write_proof "$DEST/proofs/install.json" "install" "$CID" "$SHA" "PASS" "$INST_EXTRA"
assert_tier "$DEST" "Installed"
pass "install.json + install/IRIN.app + recomputed manifest → Installed"

# --- bare/schema-invalid acceptance does not yield Accepted ----------------
printf '{ "result": "PASS" }\n' >"$DEST/proofs/acceptance.json"
assert_tier "$DEST" "Installed"
assert_field "$DEST" 'd["checks"]["acceptance_proof"]' "False"
pass "bare acceptance file does not yield Accepted"

# --- [P1] incomplete acceptance + incomplete/expired t2 refuse Accepted ----
python3 - "$DEST/proofs/acceptance.json" "$CID" "$SHA" "$DMG_D" "$BM_D" <<'PY'
import json, sys
# Acceptance with NO result field (previously accepted).
path, cid, sha, dmg, bm = sys.argv[1:]
doc = {
  "schema_version": 1,
  "proof_kind": "acceptance",
  "candidate_id": cid,
  "source_sha": sha,
  # result deliberately omitted
  "tool_version": "irin-test/1",
  "run_id": "acc-run",
  "timestamp": "2099-01-01T00:00:00Z",
  "dmg_sha256": dmg,
  "installed_bundle_manifest_digest": bm,
  "pending_action_id": "t2-action-test-1",
}
json.dump(doc, open(path, "w"), sort_keys=True, indent=2)
open(path, "a").write("\n")
PY
python3 - "$DEST/proofs/t2.json" "$CID" <<'PY'
import json, sys
# T2 missing source_sha, result, run metadata; expired authorization.
path, cid = sys.argv[1:]
doc = {
  "schema_version": 1,
  "proof_kind": "t2",
  "candidate_id": cid,
  "action_id": "t2-action-test-1",
  "acceptance_digest": "0" * 64,
  "authorized_effects": ["tag-push"],
  "expiry": "2000-01-01T00:00:00Z",
}
json.dump(doc, open(path, "w"), sort_keys=True, indent=2)
open(path, "a").write("\n")
PY
assert_tier "$DEST" "Installed"
assert_field "$DEST" 'd["checks"]["acceptance_proof"]' "False"
pass "incomplete acceptance/T2 + expired auth do not yield Accepted"

# --- acceptance + full t2 chain → Accepted ---------------------------------
ACTION_ID="t2-action-test-1"
ACC_EXTRA="$(python3 -c 'import json; print(json.dumps({
  "dmg_sha256": "'"$DMG_D"'",
  "installed_bundle_manifest_digest": "'"$BM_D"'",
  "pending_action_id": "'"$ACTION_ID"'",
  "installed_app_path": "/tmp/not-real.app",
}))')"
write_proof "$DEST/proofs/acceptance.json" "acceptance" "$CID" "$SHA" "PASS" "$ACC_EXTRA"
ACC_DIGEST="$(irin_sha256_file "$DEST/proofs/acceptance.json")"
write_proof "$DEST/proofs/t2.json" "t2" "$CID" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "action_id": "'"$ACTION_ID"'",
  "acceptance_digest": "'"$ACC_DIGEST"'",
  "authorized_effects": ["tag-push", "release-attach", "publish"],
  "expiry": "2099-01-01T00:00:00Z",
}))')"
assert_tier "$DEST" "Accepted"
assert_field "$DEST" 'd["checks"]["acceptance_proof"]' "True"
status_json "$DEST" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["caveats"], "expected Accepted caveat"'
pass "acceptance + full t2 envelope chain → Accepted (with human-boundary caveat)"

# --- broken t2 link does not yield Accepted --------------------------------
python3 - "$DEST/proofs/t2.json" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["acceptance_digest"] = "0" * 64
json.dump(d, open(p, "w"), sort_keys=True, indent=2)
open(p, "a").write("\n")
PY
assert_tier "$DEST" "Installed"
# restore good t2
write_proof "$DEST/proofs/t2.json" "t2" "$CID" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
  "action_id": "'"$ACTION_ID"'",
  "acceptance_digest": "'"$ACC_DIGEST"'",
  "authorized_effects": ["tag-push", "release-attach", "publish"],
  "expiry": "2099-01-01T00:00:00Z",
}))')"
assert_tier "$DEST" "Accepted"
pass "broken t2 acceptance_digest link refuses Accepted"

# --- [P1] symlink lexically under store but physically outside refuses -----
OUTSIDE="$TEST_HOME/outside-candidate"
rm -rf "$OUTSIDE"
mkdir -p "$OUTSIDE"
# Build a minimal outside tree that looks like a candidate (copy from DEST)
cp -R "$DEST/." "$OUTSIDE/"
chmod -R u+w "$OUTSIDE" 2>/dev/null || true
LINK_PATH="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA/symlink-escape"
rm -rf "$LINK_PATH"
ln -s "$OUTSIDE" "$LINK_PATH"
set +e
out="$("$STATUS" --candidate "$LINK_PATH" --json 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "symlink escape should refuse (exit $ec): $out"
[[ "$out" == *"physical"* || "$out" == *"IRIN_CANDIDATE_ROOT"* ]] \
  || fail "expected physical containment refuse: $out"
pass "symlink lexically under store but physically outside is refused"

# --- [P1] overrides ignored without HERMETIC=1 / real-store shape ----------
# Simulate a non-hermetic invocation: drop HERMETIC, keep green overrides.
# With HERMETIC unset, reporter must not trust overrides. Stub gh + refuse
# git fetch so this case stays zero-network on hosted detect-changes (facts
# become unavailable rather than Candidate verified).
STUB_BIN="$(mktemp -d "$TEST_HOME/stub-bin.XXXXXX")"
REAL_GIT="$(command -v git)"
cat >"$STUB_BIN/gh" <<'EOF'
#!/bin/sh
fixture="${IRIN_CANDIDATE_STATUS_TEST_GH_FIXTURE:-}"
if [ -n "$fixture" ]; then
  for arg in "$@"; do
    if [ "$arg" = "--jq" ]; then
      printf '{"status":"completed","conclusion":"success"}\n'
      exit 0
    fi
  done
fi
case "$fixture" in
  later_failure)
    cat <<'JSON'
{"check_runs":[{"name":"CI required","status":"completed","conclusion":"success","started_at":"2026-08-24T10:00:00Z","completed_at":"2026-08-24T10:05:00Z"},{"name":"ci / CI required","status":"completed","conclusion":"failure","started_at":"2026-08-24T11:00:00Z","completed_at":"2026-08-24T11:05:00Z"}]}
JSON
    ;;
  latest_in_progress)
    cat <<'JSON'
{"check_runs":[{"name":"CI required","status":"completed","conclusion":"success","started_at":"2026-08-24T10:00:00Z","completed_at":"2026-08-24T10:05:00Z"},{"name":"CI required","status":"in_progress","conclusion":null,"started_at":"2026-08-24T11:00:00Z","completed_at":null}]}
JSON
    ;;
  *)
    exit 1
    ;;
esac
EOF
cat >"$STUB_BIN/git" <<EOF
#!/bin/sh
for a in "\$@"; do
  if [ "\$a" = "fetch" ]; then
    echo "stub-git: fetch refused (hermetic candidate-status contract)" >&2
    exit 1
  fi
done
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$STUB_BIN/gh" "$STUB_BIN/git"

assert_live_ci_false() {
  local fixture="$1" label="$2" out ci tier
  out="$(
    env -u IRIN_CANDIDATE_STATUS_CI_REQUIRED \
      PATH="$STUB_BIN:$PATH" \
      IRIN_CANDIDATE_STATUS_HERMETIC=1 \
      IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
      IRIN_CANDIDATE_STATUS_TEST_GH_FIXTURE="$fixture" \
      IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
      "$STATUS" --candidate "$DEST" --json
  )"
  ci="$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["checks"]["ci_required_green"])')"
  tier="$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tier") or "")')"
  [[ "$ci" == "false" ]] || fail "$label must report CI required false (got $ci)"
  [[ -z "$tier" ]] || fail "$label must stay below Candidate verified (got $tier)"
  pass "$label leaves candidate below Candidate verified"
}

assert_live_ci_false "later_failure" "later CI required failure beats earlier success"
assert_live_ci_false "latest_in_progress" "latest in-progress CI required run fails closed"

set +e
out="$(
  PATH="$STUB_BIN:$PATH" \
  IRIN_CANDIDATE_STATUS_HERMETIC='' \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$STATUS" --candidate "$DEST" --json 2>/dev/null
)"
ec=$?
set -e
[[ $ec -eq 0 ]] || fail "non-hermetic status should still report well-formed candidate"
tier="$(printf '%s' "$out" | python3 -c 'import json,sys; t=json.load(sys.stdin).get("tier"); print(t if t else "")')"
[[ -z "$tier" ]] || fail "overrides without HERMETIC must not force Candidate verified (got $tier)"
# restore hermetic greens
export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true
assert_tier "$DEST" "Accepted"
pass "SOURCE_ON_MAIN/CI_REQUIRED overrides ignored without HERMETIC+temp-store"

# --- publication → Published -----------------------------------------------
PUB_EXTRA="$(python3 -c 'import json; print(json.dumps({
  "public_state": "published",
  "redownload_unauthenticated": True,
  "asset_sha256": "'"$DMG_D"'",
  "tag": "v0.1.2",
  "release_url": "https://github.com/irinityhq/irin/releases/tag/v0.1.2",
}))')"
write_proof "$DEST/proofs/publication.json" "publication" "$CID" "$SHA" "PASS" "$PUB_EXTRA"
assert_tier "$DEST" "Published"
"$STATUS" --candidate "$DEST" --require "Published" >/dev/null
pass "publication proof with unauthenticated re-download hash → Published"

# --- adapter fixtures pin required keys for W3 -----------------------------
for f in \
  "$FIXTURES/example-candidate-verified.json" \
  "$FIXTURES/example-below-verified.json" \
  "$FIXTURES/example-accepted.json"
do
  [[ -f "$f" ]] || fail "missing fixture: $f"
  validate_schema_shape "$f"
done
# Live output keys match fixture key set
python3 - "$TEST_HOME/verified.json" "$FIXTURES/example-candidate-verified.json" <<'PY'
import json, sys
live = set(json.load(open(sys.argv[1])))
fix = set(json.load(open(sys.argv[2])))
assert live == fix, f"live keys {sorted(live)} != fixture keys {sorted(fix)}"
print("adapter key set matches fixture")
PY
pass "JSON fixtures pin adapter contract keys for W3"

# --- text mode prints tier -------------------------------------------------
text_out="$("$STATUS" --candidate "$DEST")"
[[ "$text_out" == *"tier: Published"* ]] || fail "text mode should print tier: $text_out"
pass "text mode prints tier and blockers"

printf '\nAll candidate-status contracts passed.\n'
