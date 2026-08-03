#!/usr/bin/env bash
# W5 hermetic: remove-worktree harvest + incomplete-evidence refuse.
# Zero network. Uses a real temporary git worktree under this monorepo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-w5-harvest.XXXXXX")"
BRANCH="test/w5-harvest-$$"
WT=""
cleanup() {
  if [[ -n "$WT" && -d "$WT" ]]; then
    # Best-effort: force-remove residual worktree registration.
    git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  fi
  git -C "$ROOT" branch -D "$BRANCH" 2>/dev/null || true
  if [[ -d "$TEST_HOME" ]]; then
    chmod -R u+w "$TEST_HOME" 2>/dev/null || true
    rm -rf "$TEST_HOME"
  fi
}
trap cleanup EXIT

export IRIN_CANDIDATE_ROOT="$TEST_HOME/candidates"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

REMOVE="$ROOT/scripts/remove-worktree.sh"
[[ -x "$REMOVE" ]] || fail "remove-worktree.sh not executable"

# Temp worktree on a disposable branch (clean tree).
WT="$TEST_HOME/wt"
git -C "$ROOT" worktree add -q -b "$BRANCH" "$WT" HEAD \
  || fail "could not create temp worktree"

# Store-side candidate that must survive worktree removal.
SHA="$(python3 -c "print(('d' * 40)[:40])")"
S1="$TEST_HOME/stage"
mkdir -p "$S1/IRIN.app/Contents/MacOS" "$S1/proofs" "$S1/smoke" "$S1/install" "$S1/logs"
printf 'host' >"$S1/IRIN.app/Contents/MacOS/council-warroom-tauri"
printf 'side' >"$S1/IRIN.app/Contents/MacOS/council"
printf 'dmg-store-bytes' >"$S1/IRIN_0.0.1_aarch64.dmg"
irin_write_bundle_manifest "$S1/IRIN.app" "$S1/bundle-manifest.txt"
bm_d="$(irin_sha256_file "$S1/bundle-manifest.txt")"
dmg_d="$(irin_sha256_file "$S1/IRIN_0.0.1_aarch64.dmg")"
app_d="$(irin_sha256_file "$S1/IRIN.app/Contents/MacOS/council-warroom-tauri")"
cat >"$S1/HASHES.txt" <<EOF
pack_mode=local-dev
release_version=0.0.1
releasable=false
stapled=false
source_sha=$SHA
build_dirty=false
arch=aarch64-apple-darwin
app=IRIN.app
dmg=IRIN_0.0.1_aarch64.dmg
app_sha256=$app_d
council_sha256=$(irin_sha256_file "$S1/IRIN.app/Contents/MacOS/council")
arm_attest_sha256=$(printf 'x' | irin_sha256_bytes)
gateway_pack_compose_sha256=$(printf 'y' | irin_sha256_bytes)
gateway_pack_manifest_sha256=$(printf 'z' | irin_sha256_bytes)
gateway_digest=$(python3 -c 'print("g"+"0"*63)')
sidecar_digest=$(python3 -c 'print("s"+"0"*63)')
warroom_web_index_sha256=$(printf 'w' | irin_sha256_bytes)
bundle_manifest_digest=$bm_d
dmg_sha256=$dmg_d
EOF
python3 - "$S1/candidate.json" "$SHA" "$bm_d" "$dmg_d" <<'PY'
import json, sys
out, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.0.1",
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
CID="$(irin_sha256_file "$S1/candidate.json")"
STORE_DEST="$IRIN_CANDIDATE_ROOT/0.0.1/$SHA/$CID"
irin_promote_candidate_from_staging "$S1" "$STORE_DEST" >/dev/null
[[ -d "$STORE_DEST" ]] || fail "store candidate missing after promote"

# --- incomplete ignored residue refuses removal ----------------------------
mkdir -p "$WT/packaging/artifacts/legacy-cand"
printf 'HASHES-only residue\n' >"$WT/packaging/artifacts/legacy-cand/HASHES.txt"
# PATH for worktree commands: ensure scripts resolve.
set +e
out="$(
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
  "$REMOVE" "$WT" 2>&1
)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "incomplete residue must refuse removal: $out"
[[ "$out" == *"incomplete"* || "$out" == *"legacy"* || "$out" == *"refusing"* ]] \
  || fail "expected incomplete refuse message: $out"
[[ -d "$WT" ]] || fail "worktree must still exist after refuse"
[[ -d "$STORE_DEST" ]] || fail "store candidate must survive refused removal"
pass "incomplete ignored evidence refuses worktree removal"

# Clean incomplete residue.
rm -rf "$WT/packaging/artifacts/legacy-cand"

# --- complete ignored candidate harvests into store, then removes ----------
# Build a complete payload under packaging/artifacts (ignored scan root).
EV="$WT/packaging/artifacts/harvest-cand"
mkdir -p "$EV/IRIN.app/Contents/MacOS" "$EV/proofs" "$EV/smoke" "$EV/install" "$EV/logs"
printf 'host2' >"$EV/IRIN.app/Contents/MacOS/council-warroom-tauri"
printf 'side2' >"$EV/IRIN.app/Contents/MacOS/council"
printf 'dmg-harvest-bytes' >"$EV/IRIN_0.0.2_aarch64.dmg"
irin_write_bundle_manifest "$EV/IRIN.app" "$EV/bundle-manifest.txt"
bm2="$(irin_sha256_file "$EV/bundle-manifest.txt")"
dmg2="$(irin_sha256_file "$EV/IRIN_0.0.2_aarch64.dmg")"
app2="$(irin_sha256_file "$EV/IRIN.app/Contents/MacOS/council-warroom-tauri")"
SHA2="$(python3 -c "print(('e' * 40)[:40])")"
cat >"$EV/HASHES.txt" <<EOF
pack_mode=local-dev
release_version=0.0.2
releasable=false
stapled=false
source_sha=$SHA2
build_dirty=false
arch=aarch64-apple-darwin
app=IRIN.app
dmg=IRIN_0.0.2_aarch64.dmg
app_sha256=$app2
council_sha256=$(irin_sha256_file "$EV/IRIN.app/Contents/MacOS/council")
arm_attest_sha256=$(printf 'x' | irin_sha256_bytes)
gateway_pack_compose_sha256=$(printf 'y' | irin_sha256_bytes)
gateway_pack_manifest_sha256=$(printf 'z' | irin_sha256_bytes)
gateway_digest=$(python3 -c 'print("g"+"1"*63)')
sidecar_digest=$(python3 -c 'print("s"+"1"*63)')
warroom_web_index_sha256=$(printf 'w' | irin_sha256_bytes)
bundle_manifest_digest=$bm2
dmg_sha256=$dmg2
EOF
python3 - "$EV/candidate.json" "$SHA2" "$bm2" "$dmg2" <<'PY'
import json, sys
out, source_sha, bm_d, dmg_d = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": "0.0.2",
  "pack_mode": "local-dev",
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": False,
  "gateway_digest": "g" + ("1" * 63),
  "sidecar_digest": "s" + ("1" * 63),
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
CID2="$(irin_sha256_file "$EV/candidate.json")"
HARVEST_DEST="$IRIN_CANDIDATE_ROOT/0.0.2/$SHA2/$CID2"
[[ ! -d "$HARVEST_DEST" ]] || fail "harvest dest must not pre-exist"

# packaging/artifacts is typically gitignored — ensure no dirty status from
# untracked files by checking remove-worktree's porcelain check. It uses
# --untracked-files=normal, so untracked artifacts WILL dirty the worktree.
# Force-add ignore if needed: packaging/artifacts should already be ignored.
if [[ -n "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]]; then
  # If artifacts are not ignored, stage nothing — instead write under a path
  # we force via IRIN_WORKTREE_EVIDENCE_SCAN_ROOTS that is gitignored.
  # Prefer ensuring packaging/artifacts is ignored.
  if git -C "$WT" check-ignore -q "packaging/artifacts/harvest-cand/candidate.json" 2>/dev/null; then
    : # ignored — but porcelain may still show other dirt; clean it.
    git -C "$WT" status --porcelain --untracked-files=normal >&2 || true
  fi
fi

# Confirm ignored (not dirtying).
if [[ -n "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]]; then
  # Create evidence under an extra ignored scan root inside the worktree.
  mkdir -p "$WT/.irin-receipts/harvest-cand"
  # Move payload into .irin-receipts (also a scan root).
  rm -rf "$WT/.irin-receipts/harvest-cand"
  mv "$EV" "$WT/.irin-receipts/harvest-cand"
  EV="$WT/.irin-receipts/harvest-cand"
  # If still dirty, the tree itself is dirty from the worktree add? Unlikely.
  if [[ -n "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]]; then
    # Last resort: mark as assume-unchanged won't help untracked. Write a
    # local exclude for the scan path.
    mkdir -p "$WT/.git/info" 2>/dev/null || true
    # Linked worktrees use $ROOT/.git/worktrees/...
    git -C "$WT" status --porcelain --untracked-files=normal | head -20 >&2 || true
    # Use git clean? No — we need the files. Add to exclude:
    exclude_file="$(git -C "$WT" rev-parse --git-dir)/info/exclude"
    mkdir -p "$(dirname "$exclude_file")"
    printf '%s\n' '.irin-receipts/' 'packaging/artifacts/' >>"$exclude_file"
  fi
fi

[[ -z "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]] \
  || fail "worktree still dirty before remove: $(git -C "$WT" status --porcelain)"

out="$(
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
  "$REMOVE" "$WT" 2>&1
)" || fail "remove with complete evidence should succeed: $out"
[[ "$out" == *"Harvested"* || "$out" == *"harvest"* || -d "$HARVEST_DEST" ]] \
  || fail "expected harvest note or dest: $out"
[[ -d "$HARVEST_DEST" ]] || fail "harvested candidate missing at $HARVEST_DEST"
[[ -f "$HARVEST_DEST/candidate.json" ]] || fail "harvested candidate.json missing"
[[ ! -d "$WT" ]] || fail "worktree should be gone after successful remove"
[[ -d "$STORE_DEST" ]] || fail "pre-existing store candidate must survive"
# Mark WT empty so cleanup does not double-remove.
WT=""
pass "complete ignored evidence harvests into store; direct store survives"

printf '\nAll remove-worktree evidence contracts passed.\n'
