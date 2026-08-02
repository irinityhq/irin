#!/usr/bin/env bash
# Hermetic contracts for export-candidate / import-candidate (no Apple, no network).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-export-import-test.XXXXXX")"
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

EXPORT="$ROOT/scripts/export-candidate.sh"
IMPORT="$ROOT/scripts/import-candidate.sh"
[[ -x "$EXPORT" && -x "$IMPORT" ]] || fail "export/import scripts not executable"

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
  # Disposable trees must not affect archive identity of payload.
  echo "noise" >"$staging/logs/build.txt"
  echo "smoke" >"$staging/smoke/x.txt"
  local cid extra
  cid="$(irin_sha256_file "$staging/candidate.json")"
  extra="$(python3 -c 'import json,sys; print(json.dumps({"dmg_sha256":sys.argv[1],"bundle_manifest_digest":sys.argv[2],"pack_mode":sys.argv[3]}))' \
    "$dmg_d" "$bm_d" "$pack_mode")"
  irin_write_proof_envelope \
    "$staging/proofs/verify.json" \
    "verify" \
    "$cid" \
    "$source_sha" \
    "PASS" \
    "$extra"
}

SHA_A="$(sha40 a)"
STAGE="$TEST_HOME/stage"
make_staging "$STAGE" "local-dev" "$SHA_A" "dmg-body-A"
CID="$(irin_sha256_file "$STAGE/candidate.json")"
DEST="$IRIN_CANDIDATE_ROOT/0.1.2/$SHA_A/$CID"
R="$(irin_promote_candidate_from_staging "$STAGE" "$DEST")"
[[ "$R" == "created" ]] || fail "promote expected created, got $R"
# Re-open proofs for write after freeze (proof already written pre-promote).
[[ -f "$DEST/proofs/verify.json" ]] || fail "verify proof missing after promote"
PAYLOAD_BEFORE="$(irin_payload_tree_hash "$DEST")"

# --- export deterministic ----------------------------------------------------
OUT1="$TEST_HOME/export1"
OUT2="$TEST_HOME/export2"
EXP1="$("$EXPORT" --candidate "$DEST" --output "$OUT1")"
ARCHIVE1="$(sed -n 's/^archive_path=//p' <<<"$EXP1")"
SHA1="$(sed -n 's/^archive_sha256=//p' <<<"$EXP1")"
[[ -f "$ARCHIVE1" ]] || fail "archive missing"
[[ -f "${ARCHIVE1}.sha256" ]] || fail "sidecar missing"
[[ "$SHA1" =~ ^[0-9a-f]{64}$ ]] || fail "bad archive sha"
# Disposable trees excluded from archive (check listing).
python3 - "$ARCHIVE1" <<'PY' || fail "archive should exclude logs/smoke"
import gzip, io, sys, tarfile
with gzip.open(sys.argv[1], "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tar:
    names = [m.name for m in tar.getmembers()]
assert any(n == "candidate.json" or n.endswith("/candidate.json") or n == "candidate.json" for n in names)
assert any(n.endswith(".dmg") or n.endswith(".dmg") for n in names)
assert any("proofs/" in n or n.startswith("proofs") for n in names)
assert not any(n == "logs" or n.startswith("logs/") for n in names), names
assert not any(n == "smoke" or n.startswith("smoke/") for n in names), names
print("listing ok")
PY
# Second export must match byte-for-byte.
EXP2="$("$EXPORT" --candidate "$DEST" --output "$OUT2")"
ARCHIVE2="$(sed -n 's/^archive_path=//p' <<<"$EXP2")"
SHA2="$(sed -n 's/^archive_sha256=//p' <<<"$EXP2")"
[[ "$SHA1" == "$SHA2" ]] || fail "export not deterministic ($SHA1 vs $SHA2)"
cmp -s "$ARCHIVE1" "$ARCHIVE2" || fail "export archives differ"
pass "export is deterministic (archive + sha256 sidecar)"

# --- round-trip import into a fresh store ------------------------------------
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-import"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"
IMP="$("$IMPORT" --archive "$ARCHIVE1" \
  --expected-source-sha "$SHA_A" \
  --expected-candidate-id "$CID")"
IMP_PATH="$(sed -n 's/^candidate_path=//p' <<<"$IMP")"
IMP_CID="$(sed -n 's/^candidate_id=//p' <<<"$IMP")"
[[ "$IMP_CID" == "$CID" ]] || fail "import candidate-id mismatch"
[[ -f "$IMP_PATH/candidate.json" ]] || fail "import missing candidate.json"
PAYLOAD_AFTER="$(irin_payload_tree_hash "$IMP_PATH")"
[[ "$PAYLOAD_AFTER" == "$PAYLOAD_BEFORE" ]] || fail "payload tree hash changed across export/import"
[[ -f "$IMP_PATH/proofs/verify.json" ]] || fail "import lost proofs/verify.json"
pass "export/import round-trips exact candidate-id + payload"

# Idempotent re-import of the same archive.
IMP2="$("$IMPORT" --archive "$ARCHIVE1" --expected-candidate-id "$CID")"
[[ "$(sed -n 's/^promote_result=//p' <<<"$IMP2")" == "idempotent" ]] \
  || fail "re-import should be idempotent"
pass "re-import of identical archive is idempotent"

# --- refuse tampered archive -------------------------------------------------
TAMPERED="$TEST_HOME/tampered.tar.gz"
python3 - "$ARCHIVE1" "$TAMPERED" <<'PY'
import gzip, io, sys, tarfile
src, dst = sys.argv[1], sys.argv[2]
with gzip.open(src, "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tin:
    members = tin.getmembers()
    files = {m.name: tin.extractfile(m).read() if m.isfile() else None for m in members}
# Flip one byte inside candidate.json if present.
key = "candidate.json"
if key not in files or files[key] is None:
    raise SystemExit("candidate.json missing in archive")
b = bytearray(files[key])
b[0] = (b[0] ^ 0x01) & 0xFF
files[key] = bytes(b)
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tout:
    for m in members:
        if m.isfile() and m.name in files:
            data = files[m.name]
            m.size = len(data)
            tout.addfile(m, io.BytesIO(data))
        else:
            tout.addfile(m)
with gzip.GzipFile(filename="", mode="wb", fileobj=open(dst, "wb"), mtime=0) as gz:
    gz.write(buf.getvalue())
PY
set +e
out="$("$IMPORT" --archive "$TAMPERED" --expected-archive-sha256 "$SHA1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "tampered archive with wrong expected sha should refuse"
[[ "$out" == *"SHA-256 mismatch"* || "$out" == *"mismatch"* ]] \
  || fail "expected sha mismatch message: $out"
pass "tampered archive / wrong sidecar refuses"

# Import without expected sha but corrupt identity still refuses.
set +e
out="$("$IMPORT" --archive "$TAMPERED" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "corrupt candidate.json should refuse import"
pass "corrupt candidate identity refuses import"

# --- source SHA mismatch refuses ---------------------------------------------
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-import2"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"
set +e
out="$("$IMPORT" --archive "$ARCHIVE1" --expected-source-sha "$(sha40 z)" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "wrong expected source sha should refuse"
[[ "$out" == *"source_sha mismatch"* ]] || fail "expected source_sha mismatch: $out"
pass "source SHA mismatch refuses import"

# Verification PASS alone is not a tier print.
[[ "$IMP" != *"Candidate verified"* ]] || fail "import must not print Candidate verified"
pass "import does not claim Candidate verified"

printf 'export-import self-test: OK\n'
