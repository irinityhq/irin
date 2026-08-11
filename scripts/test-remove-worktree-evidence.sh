#!/usr/bin/env bash
# W5 hermetic: remove-worktree harvest + incomplete-evidence refuse +
# ship-*.txt receipt retention / collision outcomes.
# Zero network. Uses a real temporary git worktree under this monorepo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-w5-harvest.XXXXXX")"
BRANCH="test/w5-harvest-$$"
WT=""
# Unique ship receipt basenames so hermetic runs never collide with operator history.
SHIP_ABSENT="ship-w5-absent-$$.txt"
SHIP_IDENT="ship-w5-ident-$$.txt"
SHIP_DIFF="ship-w5-diff-$$.txt"
SHIP_SYMLINK="ship-w5-symlink-$$.txt"
CANON_RECEIPTS="$ROOT/.irin-receipts"
# No test may relocate or replace the real receipt root; symlink cases run
# from a disposable invoking checkout under $TEST_HOME instead.
cleanup() {
  if [[ -n "$WT" && -d "$WT" ]]; then
    # Best-effort: force-remove residual worktree registration.
    git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  fi
  git -C "$ROOT" branch -D "$BRANCH" 2>/dev/null || true
  # Drop only this run's synthetic receipts from the invoking checkout.
  if [[ -d "$CANON_RECEIPTS" && ! -L "$CANON_RECEIPTS" ]]; then
    rm -f "$CANON_RECEIPTS/$SHIP_ABSENT" \
      "$CANON_RECEIPTS/$SHIP_IDENT" \
      "$CANON_RECEIPTS/$SHIP_DIFF" \
      "$CANON_RECEIPTS/$SHIP_SYMLINK" 2>/dev/null || true
  fi
  if [[ -d "$TEST_HOME" ]]; then
    chmod -R u+w "$TEST_HOME" 2>/dev/null || true
    rm -rf "$TEST_HOME"
  fi
}
trap cleanup EXIT

# Recreate a clean linked worktree on the disposable branch (branch retained after remove).
recreate_wt() {
  if [[ -n "${WT:-}" && -d "$WT" ]]; then
    git -C "$ROOT" worktree remove --force "$WT" 2>/dev/null || true
  fi
  WT="$TEST_HOME/wt"
  rm -rf "$WT"
  git -C "$ROOT" worktree add -q "$WT" "$BRANCH" \
    || fail "could not recreate temp worktree"
  [[ -z "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]] \
    || fail "recreated worktree dirty: $(git -C "$WT" status --porcelain)"
}

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

# --- ship-*.txt receipt harvest: three collision outcomes --------------------
# Harvest destination is the invoking checkout's .irin-receipts/ (SOURCE_ROOT
# of remove-worktree.sh; operator path is the canonical checkout).

# 1) Destination absent → exact-byte copy, then worktree removed.
recreate_wt
mkdir -p "$WT/.irin-receipts"
# Keep a source fixture outside the worktree for post-remove byte compare.
printf 'IRIN SHIP RECEIPT\nstatus=PASS\nmarker=absent-%s-body\n' "$$" \
  >"$TEST_HOME/$SHIP_ABSENT"
cp -a "$TEST_HOME/$SHIP_ABSENT" "$WT/.irin-receipts/$SHIP_ABSENT"
# Ensure gitignored so remove does not dirty-refuse.
git -C "$WT" check-ignore -q ".irin-receipts/$SHIP_ABSENT" \
  || fail "ship receipt must be gitignored under .irin-receipts/"
[[ ! -e "$CANON_RECEIPTS/$SHIP_ABSENT" ]] \
  || fail "canonical absent receipt must not pre-exist: $CANON_RECEIPTS/$SHIP_ABSENT"
[[ -z "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]] \
  || fail "worktree dirty before absent-receipt remove: $(git -C "$WT" status --porcelain)"
out="$(
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
  "$REMOVE" "$WT" 2>&1
)" || fail "remove with absent ship receipt dest should succeed: $out"
[[ ! -d "$WT" ]] || fail "worktree should be gone after absent-receipt harvest"
WT=""
[[ -f "$CANON_RECEIPTS/$SHIP_ABSENT" ]] \
  || fail "harvested ship receipt missing at $CANON_RECEIPTS/$SHIP_ABSENT"
cmp -s "$TEST_HOME/$SHIP_ABSENT" "$CANON_RECEIPTS/$SHIP_ABSENT" \
  || fail "harvested receipt bytes differ from source (absent dest)"
pass "ship receipt harvest: destination absent preserves exact bytes"

# 2) Destination identical → continue (no overwrite error), remove succeeds.
recreate_wt
mkdir -p "$WT/.irin-receipts" "$CANON_RECEIPTS"
printf 'IRIN SHIP RECEIPT\nstatus=PASS\nmarker=ident-%s-body\n' "$$" \
  >"$TEST_HOME/$SHIP_IDENT"
cp -a "$TEST_HOME/$SHIP_IDENT" "$WT/.irin-receipts/$SHIP_IDENT"
cp -a "$TEST_HOME/$SHIP_IDENT" "$CANON_RECEIPTS/$SHIP_IDENT"
[[ -z "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]] \
  || fail "worktree dirty before ident-receipt remove: $(git -C "$WT" status --porcelain)"
out="$(
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
  "$REMOVE" "$WT" 2>&1
)" || fail "remove with identical ship receipt dest should succeed: $out"
[[ ! -d "$WT" ]] || fail "worktree should be gone after identical-receipt continue"
WT=""
cmp -s "$TEST_HOME/$SHIP_IDENT" "$CANON_RECEIPTS/$SHIP_IDENT" \
  || fail "identical destination receipt bytes must remain unchanged"
pass "ship receipt harvest: identical destination continues"

# 3) Same name, different bytes → refuse (no overwrite, no second hierarchy).
recreate_wt
mkdir -p "$WT/.irin-receipts" "$CANON_RECEIPTS"
printf 'IRIN SHIP RECEIPT\nstatus=PASS\nmarker=wt-diff-%s-body\n' "$$" \
  >"$WT/.irin-receipts/$SHIP_DIFF"
printf 'IRIN SHIP RECEIPT\nstatus=PASS\nmarker=canon-diff-%s-body\n' "$$" \
  >"$TEST_HOME/$SHIP_DIFF"
cp -a "$TEST_HOME/$SHIP_DIFF" "$CANON_RECEIPTS/$SHIP_DIFF"
[[ -z "$(git -C "$WT" status --porcelain --untracked-files=normal)" ]] \
  || fail "worktree dirty before diff-receipt remove: $(git -C "$WT" status --porcelain)"
set +e
out="$(
  IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
  "$REMOVE" "$WT" 2>&1
)"
ec=$?
set -e
[[ $ec -eq 1 ]] || fail "different-content ship receipt must exit 1 (got $ec): $out"
[[ "$out" == *"refus"* || "$out" == *"different"* || "$out" == *"overwrite"* || "$out" == *"collision"* ]] \
  || fail "expected different-content refuse message: $out"
[[ -d "$WT" ]] || fail "worktree must still exist after receipt collision refuse"
# Canonical bytes must be untouched; no alternate hierarchy (e.g. ship-*-wt or subdir).
cmp -s "$TEST_HOME/$SHIP_DIFF" "$CANON_RECEIPTS/$SHIP_DIFF" \
  || fail "canonical receipt must not be overwritten on collision"
# No second-hierarchy spill under .irin-receipts for this basename.
spill_count="$(
  find "$CANON_RECEIPTS" -name "${SHIP_DIFF}*" 2>/dev/null | wc -l | tr -d ' '
)"
[[ "$spill_count" == "1" ]] \
  || fail "expected exactly one path for colliding basename, found $spill_count"
# Worktree source still present (not destroyed mid-refuse).
[[ -f "$WT/.irin-receipts/$SHIP_DIFF" ]] \
  || fail "worktree receipt must remain after refuse"
pass "ship receipt harvest: different-content destination refuses overwrite"

# 4) Symlinked destination .irin-receipts root → refuse (no write-through).
# Run from a disposable invoking checkout so the real receipt root is never
# mutated: concurrent receipts cannot be redirected, and a hard kill strands
# nothing outside $TEST_HOME.
INVOKER="$TEST_HOME/symlink-invoker"
git clone --quiet --local --no-hardlinks -- "$ROOT" "$INVOKER" \
  || fail "could not create disposable invoking checkout"
mkdir -p "$TEST_HOME/receipt-spill"
ln -s "$TEST_HOME/receipt-spill" "$INVOKER/.irin-receipts"
IWT="$TEST_HOME/symlink-invoker-wt"
git -C "$INVOKER" worktree add -q -b "test/w5-symlink-$$" "$IWT" \
  || fail "could not create disposable invoker worktree"
mkdir -p "$IWT/.irin-receipts"
printf 'IRIN SHIP RECEIPT\nstatus=PASS\nmarker=symlink-%s-body\n' "$$" \
  >"$IWT/.irin-receipts/$SHIP_SYMLINK"
[[ -z "$(git -C "$IWT" status --porcelain --untracked-files=normal)" ]] \
  || fail "invoker worktree dirty before symlink-root remove: $(git -C "$IWT" status --porcelain)"
set +e
out="$(
  cd "$INVOKER" \
    && IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
      "$REMOVE" "$IWT" 2>&1
)"
ec=$?
set -e
[[ $ec -eq 1 ]] || fail "symlinked ship receipt root must exit 1 (got $ec): $out"
[[ "$out" == *"symlink"* || "$out" == *"refus"* ]] \
  || fail "expected symlinked receipt-root refuse message: $out"
[[ -d "$IWT" ]] || fail "worktree must still exist after symlink-root refuse"
[[ ! -e "$TEST_HOME/receipt-spill/$SHIP_SYMLINK" ]] \
  || fail "must not write ship receipt through symlinked root"
[[ -f "$IWT/.irin-receipts/$SHIP_SYMLINK" ]] \
  || fail "worktree receipt must remain after symlink-root refuse"
pass "ship receipt harvest: symlinked destination root refuses"

# 5) Non-directory destination .irin-receipts root → refuse. Reuses the
# disposable invoker and its intact worktree from the symlink case.
rm -f "$INVOKER/.irin-receipts"
printf 'not a directory\n' >"$INVOKER/.irin-receipts"
set +e
out="$(
  cd "$INVOKER" \
    && IRIN_CANDIDATE_ROOT="$IRIN_CANDIDATE_ROOT" \
      "$REMOVE" "$IWT" 2>&1
)"
ec=$?
set -e
[[ $ec -eq 1 ]] || fail "non-directory ship receipt root must exit 1 (got $ec): $out"
[[ "$out" == *"non-directory"* || "$out" == *"refus"* ]] \
  || fail "expected non-directory receipt-root refuse message: $out"
[[ -f "$IWT/.irin-receipts/$SHIP_SYMLINK" ]] \
  || fail "worktree receipt must remain after non-directory-root refuse"
pass "ship receipt harvest: non-directory destination root refuses"

printf '\nAll remove-worktree evidence contracts passed.\n'
