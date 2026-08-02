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
    "$staging/IRIN.app/Contents/Frameworks" \
    "$staging/proofs" "$staging/smoke" "$staging/install" "$staging/logs"
  printf 'host' >"$staging/IRIN.app/Contents/MacOS/council-warroom-tauri"
  printf 'side' >"$staging/IRIN.app/Contents/MacOS/council"
  # Executable bits must be recorded in the manifest so mode-drop corruption fails.
  chmod 0755 "$staging/IRIN.app/Contents/MacOS/council-warroom-tauri" \
    "$staging/IRIN.app/Contents/MacOS/council"
  # Framework-style directory symlink (must round-trip as symlink, not dir).
  mkdir -p "$staging/IRIN.app/Contents/Frameworks/Real.framework/Versions/A"
  printf 'fw' >"$staging/IRIN.app/Contents/Frameworks/Real.framework/Versions/A/Real"
  ln -s "A" "$staging/IRIN.app/Contents/Frameworks/Real.framework/Versions/Current"
  ln -s "Versions/Current/Real" "$staging/IRIN.app/Contents/Frameworks/Real.framework/Real"
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
[[ -f "$DEST/proofs/verify.json" ]] || fail "verify proof missing after promote"
# Attach install witnesses (exact-install durable path).
mkdir -p "$DEST/install/IRIN.app/Contents/MacOS"
# install/ is writable; copy from frozen app via ditto/cp that re-enables write.
chmod -R u+w "$DEST/install" 2>/dev/null || true
cp -a "$DEST/IRIN.app/." "$DEST/install/IRIN.app/"
chmod -R u+w "$DEST/install/IRIN.app"
cp "$DEST/bundle-manifest.txt" "$DEST/install/bundle-manifest.txt"
BM_D="$(irin_sha256_file "$DEST/bundle-manifest.txt")"
INST_EXTRA="$(python3 -c 'import json,sys; print(json.dumps({
  "candidate_bundle_manifest_digest": sys.argv[1],
  "installed_bundle_manifest_digest": sys.argv[1],
}))' "$BM_D")"
irin_write_proof_envelope \
  "$DEST/proofs/install.json" \
  "install" \
  "$CID" \
  "$SHA_A" \
  "PASS" \
  "$INST_EXTRA"
PAYLOAD_BEFORE="$(irin_payload_tree_hash "$DEST")"

# --- export deterministic ----------------------------------------------------
OUT1="$TEST_HOME/export1"
OUT2="$TEST_HOME/export2"
EXP1="$("$EXPORT" --candidate "$DEST" --output "$OUT1")"
ARCHIVE1="$(sed -n 's/^archive_path=//p' <<<"$EXP1")"
SHA1="$(sed -n 's/^archive_sha256=//p' <<<"$EXP1")"
MANIFEST1="$(sed -n 's/^export_manifest=//p' <<<"$EXP1")"
[[ -f "$ARCHIVE1" ]] || fail "archive missing"
[[ -f "${ARCHIVE1}.sha256" ]] || fail "sidecar missing"
[[ -f "$MANIFEST1" ]] || fail "export manifest missing"
[[ "$SHA1" =~ ^[0-9a-f]{64}$ ]] || fail "bad archive sha"

python3 - "$ARCHIVE1" <<'PY' || fail "archive listing checks failed"
import gzip, io, os, sys, tarfile
with gzip.open(sys.argv[1], "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tar:
    members = {m.name: m for m in tar.getmembers()}
names = list(members)
assert "candidate.json" in names
assert any(n.endswith(".dmg") for n in names)
assert any(n.startswith("proofs/") for n in names)
assert "export-binding.json" in names
assert "proofs/install.json" in names
assert "install/bundle-manifest.txt" in names
assert any(n.startswith("install/IRIN.app/") for n in names)
assert not any(n == "logs" or n.startswith("logs/") for n in names), names
assert not any(n == "smoke" or n.startswith("smoke/") for n in names), names
# Directory symlink must remain a symlink member.
cur = "IRIN.app/Contents/Frameworks/Real.framework/Versions/Current"
assert cur in members, names
assert members[cur].issym(), f"{cur} should be symlink, type={members[cur].type}"
assert members[cur].linkname == "A"
print("listing ok")
PY

EXP2="$("$EXPORT" --candidate "$DEST" --output "$OUT2")"
ARCHIVE2="$(sed -n 's/^archive_path=//p' <<<"$EXP2")"
SHA2="$(sed -n 's/^archive_sha256=//p' <<<"$EXP2")"
[[ "$SHA1" == "$SHA2" ]] || fail "export not deterministic ($SHA1 vs $SHA2)"
cmp -s "$ARCHIVE1" "$ARCHIVE2" || fail "export archives differ"
pass "export is deterministic (archive + sha256 sidecar + manifest)"

# --- round-trip import into a fresh store ------------------------------------
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-import"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"
IMP="$("$IMPORT" --archive "$ARCHIVE1" \
  --export-manifest "$MANIFEST1" \
  --expected-source-sha "$SHA_A" \
  --expected-candidate-id "$CID")"
IMP_PATH="$(sed -n 's/^candidate_path=//p' <<<"$IMP")"
IMP_CID="$(sed -n 's/^candidate_id=//p' <<<"$IMP")"
[[ "$IMP_CID" == "$CID" ]] || fail "import candidate-id mismatch"
[[ -f "$IMP_PATH/candidate.json" ]] || fail "import missing candidate.json"
PAYLOAD_AFTER="$(irin_payload_tree_hash "$IMP_PATH")"
[[ "$PAYLOAD_AFTER" == "$PAYLOAD_BEFORE" ]] || fail "payload tree hash changed across export/import"
[[ -f "$IMP_PATH/proofs/verify.json" ]] || fail "import lost proofs/verify.json"
[[ -f "$IMP_PATH/proofs/install.json" ]] || fail "import lost proofs/install.json"
[[ -f "$IMP_PATH/install/bundle-manifest.txt" ]] || fail "import lost install/bundle-manifest.txt"
[[ -d "$IMP_PATH/install/IRIN.app" ]] || fail "import lost install/IRIN.app"
# Symlink round-trip
[[ -L "$IMP_PATH/IRIN.app/Contents/Frameworks/Real.framework/Versions/Current" ]] \
  || fail "directory symlink not preserved after import"
[[ "$(readlink "$IMP_PATH/IRIN.app/Contents/Frameworks/Real.framework/Versions/Current")" == "A" ]] \
  || fail "directory symlink target changed"
irin_assert_candidate_payload_matches_identity "$IMP_PATH" >/dev/null \
  || fail "imported candidate fails payload identity assert"
pass "export/import round-trips id + payload + install witnesses + dir symlinks"

# Idempotent re-import of the same archive.
IMP2="$("$IMPORT" --archive "$ARCHIVE1" --export-manifest "$MANIFEST1" --expected-candidate-id "$CID")"
[[ "$(sed -n 's/^promote_result=//p' <<<"$IMP2")" == "idempotent" ]] \
  || fail "re-import should be idempotent"
pass "re-import of identical archive is idempotent"

# Missing export manifest refuses.
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-nomf"
source "$ROOT/packaging/env.sh"
NOMF="$TEST_HOME/nomf"
mkdir -p "$NOMF"
cp "$ARCHIVE1" "$NOMF/$(basename "$ARCHIVE1")"
cp "${ARCHIVE1}.sha256" "$NOMF/$(basename "$ARCHIVE1").sha256"
set +e
out="$("$IMPORT" --archive "$NOMF/$(basename "$ARCHIVE1")" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "import without export manifest should refuse"
[[ "$out" == *"export manifest required"* || "$out" == *"missing"* ]] \
  || fail "expected missing manifest message: $out"
pass "import refuses without trusted export manifest"

# --- refuse DMG-mutated archive with matching archive SHA + unchanged IDs ----
# Adversary mutates only the DMG, rewrites archive bytes + sidecar SHA, leaves
# candidate.json / export manifest identity fields unchanged.
MUT_DIR="$TEST_HOME/mutated"
mkdir -p "$MUT_DIR"
MUT_META="$MUT_DIR/meta.txt"
python3 - "$ARCHIVE1" "$MANIFEST1" "$MUT_DIR" "$MUT_META" <<'PY'
import gzip, hashlib, io, json, os, sys, tarfile

src_archive, src_manifest, out_dir, meta_path = sys.argv[1:]
with gzip.open(src_archive, "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tin:
    members = tin.getmembers()
    files = {}
    for m in members:
        if m.isfile():
            files[m.name] = tin.extractfile(m).read()
        else:
            files[m.name] = None

dmg_name = next(n for n in files if n.endswith(".dmg") and files[n] is not None)
files[dmg_name] = files[dmg_name] + b"-MUTATED"

buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tout:
    for m in members:
        if m.isfile() and files[m.name] is not None:
            blob = files[m.name]
            m.size = len(blob)
            m.mtime = 0
            m.uid = m.gid = 0
            m.uname = m.gname = ""
            tout.addfile(m, io.BytesIO(blob))
        else:
            m.mtime = 0
            m.uid = m.gid = 0
            m.uname = m.gname = ""
            tout.addfile(m)

raw = buf.getvalue()
out_archive = os.path.join(out_dir, "mutated-" + os.path.basename(src_archive))
with open(out_archive, "wb") as fh:
    with gzip.GzipFile(filename="", mode="wb", fileobj=fh, mtime=0) as gz:
        gz.write(raw)
archive_sha = hashlib.sha256(open(out_archive, "rb").read()).hexdigest()
with open(out_archive + ".sha256", "w", encoding="utf-8") as fh:
    fh.write(f"{archive_sha}  {os.path.basename(out_archive)}\n")

# Keep identity fields; only refresh archive_sha256 so the archive hash check
# passes while payload identity still mismatches.
with open(src_manifest, encoding="utf-8") as fh:
    man = json.load(fh)
man["archive_sha256"] = archive_sha
out_man = os.path.join(out_dir, "mutated-" + os.path.basename(src_manifest))
with open(out_man, "w", encoding="utf-8") as fh:
    json.dump(man, fh, sort_keys=True, indent=2)
    fh.write("\n")
with open(meta_path, "w", encoding="utf-8") as fh:
    fh.write(out_archive + "\n")
    fh.write(out_man + "\n")
    fh.write(archive_sha + "\n")
PY
MUT_ARCHIVE="$(sed -n '1p' "$MUT_META")"
MUT_MANIFEST="$(sed -n '2p' "$MUT_META")"
MUT_SHA="$(sed -n '3p' "$MUT_META")"

export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-mut"
source "$ROOT/packaging/env.sh"
set +e
out="$("$IMPORT" --archive "$MUT_ARCHIVE" \
  --export-manifest "$MUT_MANIFEST" \
  --expected-source-sha "$SHA_A" \
  --expected-candidate-id "$CID" \
  --expected-archive-sha256 "$MUT_SHA" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "DMG-mutated archive must refuse import; got success: $out"
# Must not leave a promoted corrupt candidate.
if find "$IRIN_CANDIDATE_ROOT" -name candidate.json 2>/dev/null | grep -q .; then
  fail "mutated archive must not promote any candidate.json"
fi
[[ "$out" == *"payload"* || "$out" == *"DMG"* || "$out" == *"mismatch"* || "$out" == *"identity"* || "$out" == *"binding"* ]] \
  || fail "expected payload/identity refuse message: $out"
pass "DMG-mutated archive refuses import (payload/identity gate)"

# Tampered archive with wrong expected sha still refuses.
TAMPERED="$TEST_HOME/tampered.tar.gz"
python3 - "$ARCHIVE1" "$TAMPERED" <<'PY'
import gzip, io, sys, tarfile
src, dst = sys.argv[1], sys.argv[2]
with gzip.open(src, "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tin:
    members = tin.getmembers()
    files = {m.name: tin.extractfile(m).read() if m.isfile() else None for m in members}
b = bytearray(files["candidate.json"])
b[0] = (b[0] ^ 0x01) & 0xFF
files["candidate.json"] = bytes(b)
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tout:
    for m in members:
        if m.isfile() and m.name in files and files[m.name] is not None:
            data = files[m.name]
            m.size = len(data)
            tout.addfile(m, io.BytesIO(data))
        else:
            tout.addfile(m)
with gzip.GzipFile(filename="", mode="wb", fileobj=open(dst, "wb"), mtime=0) as gz:
    gz.write(buf.getvalue())
PY
set +e
out="$("$IMPORT" --archive "$TAMPERED" --export-manifest "$MANIFEST1" --expected-archive-sha256 "$SHA1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "tampered archive with wrong expected sha should refuse"
pass "tampered archive / wrong sidecar refuses"

# source SHA mismatch refuses
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates-import2"
source "$ROOT/packaging/env.sh"
set +e
out="$("$IMPORT" --archive "$ARCHIVE1" --export-manifest "$MANIFEST1" --expected-source-sha "$(sha40 z)" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "wrong expected source sha should refuse"
[[ "$out" == *"source_sha mismatch"* ]] || fail "expected source_sha mismatch: $out"
pass "source SHA mismatch refuses import"

[[ "$IMP" != *"Candidate verified"* ]] || fail "import must not print Candidate verified"
pass "import does not claim Candidate verified"

# --- app executable-bit corruption refuses export (W2 parity) ----------------
export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates"
source "$ROOT/packaging/env.sh"
HOST_BIN="$DEST/IRIN.app/Contents/MacOS/council-warroom-tauri"
chmod u+w "$HOST_BIN"
chmod a-x "$HOST_BIN"  # drop execute; freeze-normalized mode must fail vs stored 0755/0555
set +e
out="$(irin_assert_candidate_payload_matches_identity "$DEST" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "app mode corruption should fail payload assert: $out"
[[ "$out" == *"mode mismatch"* || "$out" == *"payload assert failed"* ]] \
  || fail "expected mode mismatch: $out"
set +e
out="$("$EXPORT" --candidate "$DEST" --output "$TEST_HOME/export-corrupt" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "export must refuse app mode corruption: $out"
# restore for cleanup
chmod u+w "$HOST_BIN" 2>/dev/null || true
chmod a+x "$HOST_BIN" 2>/dev/null || true
pass "app executable-bit corruption refuses payload assert and export"

# --- top-level IRIN.app symlink refuses (external mutable pointer) -----------
APP_REAL="$DEST/IRIN.app"
APP_BACKUP="$TEST_HOME/IRIN.app.real-backup"
EXT_APP="$TEST_HOME/external-IRIN.app"
chmod -R u+w "$DEST" 2>/dev/null || true
mv "$APP_REAL" "$APP_BACKUP"
cp -a "$APP_BACKUP" "$EXT_APP"
ln -s "$EXT_APP" "$APP_REAL"
set +e
out="$(irin_assert_candidate_payload_matches_identity "$DEST" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "top-level IRIN.app symlink should refuse: $out"
[[ "$out" == *"symlink"* || "$out" == *"must not be a symlink"* ]] \
  || fail "expected top-level symlink refuse: $out"
# restore real app
rm -f "$APP_REAL"
mv "$APP_BACKUP" "$APP_REAL"
pass "top-level IRIN.app symlink refuses payload assert"

# --- intermediate symlink must not mkdir outside store before refuse --------
BAD_ID="$(python3 -c 'print("c"*64)')"
OUTSIDE="$TEST_HOME/outside-store"
mkdir -p "$OUTSIDE" "$TEST_HOME/store-root"
# root/0.1.2 -> outside; must refuse before creating SHA dir outside the store.
ln -s "$OUTSIDE" "$TEST_HOME/store-root/0.1.2"
before_outside="$(find "$OUTSIDE" -mindepth 1 2>/dev/null | wc -l | tr -d ' ')"
set +e
out="$(irin_assert_safe_candidate_dest \
  "$TEST_HOME/store-root" '0.1.2' "$(sha40 a)" "$BAD_ID" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "intermediate symlink escape should refuse: $out"
[[ "$out" == *"symlink"* || "$out" == *"escape"* ]] \
  || fail "expected symlink/escape message: $out"
after_outside="$(find "$OUTSIDE" -mindepth 1 2>/dev/null | wc -l | tr -d ' ')"
[[ "$after_outside" == "$before_outside" ]] \
  || fail "mkdir created content outside store ($before_outside -> $after_outside): $OUTSIDE"
pass "intermediate symlink refuses without mkdir outside store"

# --- unsafe semver / non-hex source_sha --------------------------------------
mkdir -p "$TEST_HOME/safe-root"
set +e
out="$(irin_assert_safe_candidate_dest \
  "$TEST_HOME/safe-root" '../escape' "$(sha40 a)" "$BAD_ID" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "semver with ../ should refuse: $out"
[[ "$out" == *"unsafe"* || "$out" == *"invalid"* || "$out" == *"component"* || "$out" == *"escapes"* ]] \
  || fail "expected unsafe semver message: $out"
set +e
out="$(irin_assert_safe_candidate_dest \
  "$TEST_HOME/safe-root" '0.1.2' 'not-a-sha' "$BAD_ID" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "non-hex source_sha should refuse"
pass "unsafe semver/source_sha refuse safe dest helper"

printf 'export-import self-test: OK\n'
