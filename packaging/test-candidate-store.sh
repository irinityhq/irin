#!/usr/bin/env bash
# Hermetic contracts for the W1 candidate store (no Apple, no full DMG build).
# Exercises identity, deterministic HASHES/exact-retry, exclusive promote,
# failed-attempt isolation, gateway source binding, and proof envelopes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Isolate the store under a temp root (never touch the operator default).
# Keep the fixture off the monorepo (env.sh refuses store roots under checkout).
TEST_HOME="$(mktemp -d "/tmp/irin-cand-store-test.XXXXXX")"
cleanup() {
  # Frozen payload is a-w; re-enable owner write so the fixture can be removed.
  if [[ -d "$TEST_HOME" ]]; then
    chmod -R u+w "$TEST_HOME" 2>/dev/null || true
    rm -rf "$TEST_HOME"
  fi
}
trap cleanup EXIT

export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

case "$IRIN_CANDIDATE_ROOT" in
  "$TEST_HOME/candidates"|"$TEST_HOME"/candidates) ;;
  *) fail "IRIN_CANDIDATE_ROOT not pinned to test home (got $IRIN_CANDIDATE_ROOT)" ;;
esac

# --- worktree / checkout root refusal --------------------------------------
set +e
out="$(IRIN_CANDIDATE_ROOT="$ROOT/packaging/candidates-should-refuse" \
  bash -c 'source packaging/env.sh' 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "should refuse candidate root inside checkout"
[[ "$out" == *"must not be inside"* ]] || fail "refuse message missing: $out"
pass "refuse candidate root inside source checkout"

# --- canonical identity (trailing LF, key-order stable) --------------------
write_identity() {
  local out="$1" pack_mode="$2" source_sha="$3" dmg_sha="$4"
  python3 - "$out" "$pack_mode" "$source_sha" "$dmg_sha" <<'PY'
import json, sys
out, pack_mode, source_sha, dmg_sha = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.1.2",
  "pack_mode": pack_mode,
  "bundle_manifest_digest": "b" * 64,
  "dmg_sha256": dmg_sha,
  "stapled": False,
  "gateway_digest": "g" * 64,
  "sidecar_digest": "s" * 64,
}
# Intentionally unsorted input object order via intermediate dict construction.
raw = json.dumps(doc)
# Re-emit through canonical helper path in-process.
canon = json.dumps(json.loads(raw), sort_keys=True, separators=(",", ":")) + "\n"
open(out, "w", encoding="utf-8").write(canon)
print(canon, end="")
PY
}

ID1="$TEST_HOME/id1.json"
ID2="$TEST_HOME/id2.json"
SHA_A="$(python3 -c 'print("a"*40)')"
DMG_C="$(python3 -c 'print("c"*64)')"
write_identity "$ID1" "local-dev" "$SHA_A" "$DMG_C" >/dev/null
# reverse-field construction should still match after canonical write
python3 - "$ID2" "$SHA_A" "$DMG_C" <<'PY'
import json, sys
out, source_sha, dmg_sha = sys.argv[1:]
doc = {
  "stapled": False,
  "sidecar_digest": "s" * 64,
  "gateway_digest": "g" * 64,
  "dmg_sha256": dmg_sha,
  "bundle_manifest_digest": "b" * 64,
  "pack_mode": "local-dev",
  "semver": "0.1.2",
  "source_sha": source_sha,
  "schema_version": 1,
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
cmp -s "$ID1" "$ID2" || fail "canonical identity bytes differ across key order"
CID1="$(irin_sha256_file "$ID1")"
CID2="$(irin_sha256_file "$ID2")"
[[ "$CID1" == "$CID2" ]] || fail "candidate-id unstable across key order"
pass "canonical identity stable (key order + trailing LF)"

# --- build a minimal staging candidate ------------------------------------
make_staging() {
  local staging="$1" pack_mode="$2" source_sha="$3" dmg_body="$4" app_tag="${5:-}"
  rm -rf "$staging"
  mkdir -p "$staging/IRIN.app/Contents/MacOS" \
    "$staging/proofs" "$staging/smoke" "$staging/install" "$staging/logs"
  printf 'host%s' "$app_tag" >"$staging/IRIN.app/Contents/MacOS/council-warroom-tauri"
  printf 'side' >"$staging/IRIN.app/Contents/MacOS/council"
  local dmg_name="IRIN_0.1.2_aarch64.dmg"
  printf '%s' "$dmg_body" >"$staging/$dmg_name"
  irin_write_bundle_manifest "$staging/IRIN.app" "$staging/bundle-manifest.txt"
  local bm_d dmg_d app_d
  bm_d="$(irin_sha256_file "$staging/bundle-manifest.txt")"
  dmg_d="$(irin_sha256_file "$staging/$dmg_name")"
  app_d="$(irin_sha256_file "$staging/IRIN.app/Contents/MacOS/council-warroom-tauri")"
  # Deterministic HASHES (no timestamp, relative paths only).
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
  # Diagnostic-only (must NOT affect payload tree hash).
  echo "built_at=2099-01-01T00:00:00Z" >"$staging/logs/build-meta.txt"
  echo "attempt_id=test-attempt" >>"$staging/logs/build-meta.txt"
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

sha40() { python3 -c "print('$1'*40)"; }

# --- deterministic HASHES → identical payload hash across attempts --------
S1="$TEST_HOME/stage1"
S2="$TEST_HOME/stage2"
SHA_A="$(sha40 a)"
make_staging "$S1" "local-dev" "$SHA_A" "dmg-bytes-A"
make_staging "$S2" "local-dev" "$SHA_A" "dmg-bytes-A"
# Different diagnostic timestamps must not change payload hash.
echo "built_at=1999-01-01T00:00:00Z" >"$S2/logs/build-meta.txt"
H1="$(irin_payload_tree_hash "$S1")"
H2="$(irin_payload_tree_hash "$S2")"
[[ "$H1" == "$H2" ]] || fail "payload hash differs for identical immutable bytes ($H1 vs $H2)"
pass "payload tree hash ignores attempt diagnostics / is deterministic"

# Mutating DMG changes hash.
printf 'mutated' >"$S2/IRIN_0.1.2_aarch64.dmg"
# HASHES still has old dmg_sha — payload tree hashes actual DMG bytes.
H3="$(irin_payload_tree_hash "$S2")"
[[ "$H1" != "$H3" ]] || fail "payload hash did not detect DMG mutation"
pass "payload tree hash detects DMG mutation"

# --- exact-retry via exclusive promote ------------------------------------
make_staging "$S1" "local-dev" "$SHA_A" "dmg-bytes-A"
CID="$(irin_sha256_file "$S1/candidate.json")"
DEST="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_A/$CID"
R1="$(irin_promote_candidate_from_staging "$S1" "$DEST")"
[[ "$R1" == "created" ]] || fail "first promote expected created, got $R1"
[[ -f "$DEST/candidate.json" ]] || fail "dest missing candidate.json"
# Immutable payload frozen.
if [[ -w "$DEST/candidate.json" ]]; then
  fail "candidate.json should be non-writable after promote"
fi
if [[ -w "$DEST/IRIN.app/Contents/MacOS/council" ]]; then
  fail "IRIN.app should be non-writable after promote"
fi
# proofs/ remains writable for tier evidence.
[[ -d "$DEST/proofs" ]] || fail "proofs/ missing after promote"
touch "$DEST/proofs/.writable_ok" || fail "proofs/ should remain writable"
pass "promote creates dest and freezes immutable payload (proofs still writable)"

# Exact retry with identical staging → idempotent (not corruption).
make_staging "$S2" "local-dev" "$SHA_A" "dmg-bytes-A"
# Different attempt diagnostics
echo "built_at=2000-01-01T00:00:00Z" >"$S2/logs/build-meta.txt"
R2="$(irin_promote_candidate_from_staging "$S2" "$DEST")"
[[ "$R2" == "idempotent" ]] || fail "exact retry expected idempotent, got $R2"
pass "exact retry is idempotent when immutable payload matches"

# Different payload under same candidate-id → corruption refuse.
make_staging "$S2" "local-dev" "$SHA_A" "dmg-bytes-DIFFERENT"
# Hostile collision: reuse dest identity docs with different app bytes.
cp "$DEST/candidate.json" "$S2/candidate.json"
cp "$DEST/HASHES.txt" "$S2/HASHES.txt"
cp "$DEST/bundle-manifest.txt" "$S2/bundle-manifest.txt"
cp "$DEST/IRIN_0.1.2_aarch64.dmg" "$S2/IRIN_0.1.2_aarch64.dmg"
printf 'hostile-app' >"$S2/IRIN.app/Contents/MacOS/council-warroom-tauri"
set +e
out="$(irin_promote_candidate_from_staging "$S2" "$DEST" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "corrupt collision should refuse"
[[ "$out" == *"corruption"* ]] || fail "expected corruption message: $out"
pass "payload mismatch under same candidate-id refuses (corruption)"

# --- concurrent claim / incomplete dest / atomic rename -------------------
SHA_B="$(sha40 b)"
make_staging "$S1" "signed-rc" "$SHA_B" "dmg-B"
CIDB="$(irin_sha256_file "$S1/candidate.json")"
DESTB="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_B/$CIDB"
CLAIMB="${DESTB}.claim"

# Incomplete final path (legacy/non-atomic residue) must refuse without nesting.
mkdir -p "$DESTB"
make_staging "$S2" "signed-rc" "$SHA_B" "dmg-B"
set +e
out="$(irin_promote_candidate_from_staging "$S2" "$DESTB" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "incomplete concurrent dest should refuse"
[[ "$out" == *"incomplete"* ]] || fail "expected incomplete message: $out"
[[ ! -d "$DESTB/$CIDB" ]] || fail "staging was nested under existing dest"
[[ ! -e "$DESTB/IRIN.app" ]] || fail "staging contents leaked into incomplete dest"
rm -rf "$DESTB"
pass "incomplete dest refuses without nesting"

# Stale/concurrent sibling claim with no final path blocks promote.
mkdir -p "$CLAIMB"
make_staging "$S2" "signed-rc" "$SHA_B" "dmg-B"
set +e
out="$(irin_promote_candidate_from_staging "$S2" "$DESTB" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "stale claim should refuse"
[[ "$out" == *"claim"* ]] || fail "expected claim message: $out"
[[ ! -e "$DESTB" ]] || fail "final path must not appear under a blocking claim"
rm -rf "$CLAIMB"
pass "concurrent/stale sibling claim refuses without creating final path"

# Atomic rename: final path appears only as the full staging tree (not empty shell).
make_staging "$S1" "signed-rc" "$SHA_B" "dmg-B"
# Staging must be frozen before rename — after created, payload is non-writable.
R_AT="$(irin_promote_candidate_from_staging "$S1" "$DESTB")"
[[ "$R_AT" == "created" ]] || fail "atomic promote expected created, got $R_AT"
[[ -f "$DESTB/candidate.json" && -d "$DESTB/IRIN.app" && -f "$DESTB/HASHES.txt" ]] \
  || fail "atomic rename did not yield a full candidate tree"
[[ ! -e "$CLAIMB" ]] || fail "claim must be released after successful promote"
[[ ! -d "$S1" ]] || fail "staging path must be gone after atomic rename (became dest)"
if [[ -w "$DESTB/candidate.json" || -w "$DESTB/IRIN.app/Contents/MacOS/council" ]]; then
  fail "payload must be non-writable immediately after promote (freeze-before-rename)"
fi
pass "atomic rename yields full frozen candidate; claim released; staging path gone"

# Crash residue: complete-but-writable final path is healed on idempotent retry.
make_staging "$S2" "signed-rc" "$SHA_B" "dmg-B"
# Simulate pre-freeze crash residue: force payload writable while keeping bytes.
chmod -R u+w "$DESTB/IRIN.app" "$DESTB/candidate.json" "$DESTB/HASHES.txt" \
  "$DESTB/bundle-manifest.txt" "$DESTB"/*.dmg 2>/dev/null || true
if [[ ! -w "$DESTB/candidate.json" ]]; then
  fail "setup: expected writable crash-residue candidate.json"
fi
R_HEAL="$(irin_promote_candidate_from_staging "$S2" "$DESTB")"
[[ "$R_HEAL" == "idempotent" ]] || fail "expected idempotent heal, got $R_HEAL"
if [[ -w "$DESTB/candidate.json" || -w "$DESTB/IRIN.app/Contents/MacOS/council" ]]; then
  fail "idempotent path must re-freeze crash-residue writable payload"
fi
pass "idempotent path re-freezes complete-but-writable crash residue"

# --- two same-version different-SHA coexist; two modes one SHA ------------
SHA_C="$(sha40 c)"
SHA_D="$(sha40 d)"
make_staging "$S1" "local-dev" "$SHA_C" "dmg-C1"
make_staging "$S2" "local-dev" "$SHA_D" "dmg-C2"
C1="$(irin_sha256_file "$S1/candidate.json")"
C2="$(irin_sha256_file "$S2/candidate.json")"
D1="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_C/$C1"
D2="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_D/$C2"
[[ "$(irin_promote_candidate_from_staging "$S1" "$D1")" == "created" ]]
[[ "$(irin_promote_candidate_from_staging "$S2" "$D2")" == "created" ]]
[[ -d "$D1" && -d "$D2" ]] || fail "coexist failed"
[[ "$C1" != "$C2" ]] || fail "different SHAs should yield different ids"
pass "two same-version different-SHA candidates coexist"

SHA_E="$(sha40 e)"
make_staging "$S1" "local-dev" "$SHA_E" "dmg-E1"
make_staging "$S2" "signed-rc" "$SHA_E" "dmg-E2"
C1="$(irin_sha256_file "$S1/candidate.json")"
C2="$(irin_sha256_file "$S2/candidate.json")"
[[ "$C1" != "$C2" ]] || fail "different pack modes should not collide"
D1="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_E/$C1"
D2="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_E/$C2"
[[ "$(irin_promote_candidate_from_staging "$S1" "$D1")" == "created" ]]
[[ "$(irin_promote_candidate_from_staging "$S2" "$D2")" == "created" ]]
pass "two different builds from one SHA do not collide"

# --- failed attempt path is not a valid candidate -------------------------
FAIL_DIR="$IRIN_CANDIDATE_ROOT/0.1.2/$(sha40 f)/failed/attempt-1"
mkdir -p "$FAIL_DIR"
echo x >"$FAIL_DIR/note"
set +e
out="$(IRIN_CANDIDATE_PATH="$FAIL_DIR" bash -c 'source packaging/env.sh; irin_require_candidate_path' 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "failed path should not be accepted as candidate"
[[ "$out" == *"failed attempt"* ]] || fail "expected failed-attempt refuse: $out"
pass "failed attempt path cannot be reused as candidate"

# --- gateway source binding -----------------------------------------------
MAN="$TEST_HOME/manifest.json"
python3 - "$MAN" <<'PY'
import json, sys
doc = {
  "schema_version": 1,
  "mode": "local-dev",
  "source_sha": "a" * 40,
  "images": {
    "gateway": "irin-desktop/gateway@sha256:" + ("1" * 64),
    "sidecar": "irin-desktop/sidecar@sha256:" + ("2" * 64),
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": False,
    "WATCH_DISPATCHER_ENABLED": False,
  },
}
json.dump(doc, open(sys.argv[1], "w"))
PY
irin_assert_gateway_source_binding "$MAN" "$(sha40 a)" "signed-rc" >/dev/null \
  || fail "signed-rc should accept matching local-dev source_sha"
set +e
out="$(irin_assert_gateway_source_binding "$MAN" "$(sha40 b)" "signed-rc" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "signed-rc must refuse mismatched source_sha"
[[ "$out" == *"not source-bound"* ]] || fail "expected source-bound message: $out"
pass "signed-rc Gateway inputs require matching source_sha"

# --- proof envelope atomic write ------------------------------------------
PROOF="$D1/proofs/verify.json"
EXTRA_JSON="$(python3 -c 'import json; print(json.dumps({"dmg_sha256":"0"*64,"tool":"test"}))')"
irin_write_proof_envelope \
  "$PROOF" \
  "verify" \
  "$C1" \
  "$SHA_E" \
  "PASS" \
  "$EXTRA_JSON"
[[ -f "$PROOF" ]] || fail "proof missing"
python3 - "$PROOF" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["schema_version"] == 1
assert d["proof_kind"] == "verify"
assert d["result"] == "PASS"
assert d["candidate_id"]
assert d["source_sha"]
assert d["run_id"]
assert d["timestamp"]
assert d["tool"] == "test"
print("proof envelope schema ok")
PY
pass "proof envelope written atomically with required bindings"

# --- IRIN_CANDIDATE_PATH required by verify/smoke entrypoints -------------
set +e
out="$(unset IRIN_CANDIDATE_PATH; bash packaging/verify-dmg.sh 2>&1 | head -3)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "verify must require IRIN_CANDIDATE_PATH"
pass "verify-dmg refuses without IRIN_CANDIDATE_PATH"

set +e
out="$(unset IRIN_CANDIDATE_PATH; bash packaging/smoke-full-app.sh 2>&1 | head -3)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "smoke must require IRIN_CANDIDATE_PATH"
pass "smoke-full-app refuses without IRIN_CANDIDATE_PATH"

set +e
out="$(IRIN_CANDIDATE_PATH=/tmp IRIN_SMOKE_APP=/tmp/x bash packaging/smoke-full-app.sh 2>&1 | head -3)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "smoke must forbid IRIN_SMOKE_APP"
[[ "$out" == *"IRIN_SMOKE_APP is forbidden"* ]] || fail "expected IRIN_SMOKE_APP refuse: $out"
pass "smoke-full-app forbids IRIN_SMOKE_APP bypass"

printf '\nAll candidate-store contracts passed.\n'
