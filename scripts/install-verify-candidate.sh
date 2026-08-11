#!/usr/bin/env bash
# install-verify-candidate.sh — fresh-extract the named candidate DMG into
# candidate/install/ and write proofs/install.json when digests match.
#
# Digests only — not Arm/Watch product behavior.
#
# Usage:
#   scripts/install-verify-candidate.sh --candidate ABSOLUTE_STORE_PATH
#   scripts/install-verify-candidate.sh --candidate ABSOLUTE_STORE_PATH --live
#
# Default (no --live): extract + candidate-local install proof only.
# --live (opt-in): after extract verify, staged-swap the verified extract into
# /Applications/IRIN.app (or a hermetic Applications root), then write install
# proof with live_* fields only on full success.
#
# Refuses:
#   - path outside IRIN_CANDIDATE_ROOT
#   - missing DMG / candidate.json
#   - installed vs candidate bundle-manifest digest divergence
#   - --live with pack_mode=local-dev (ad-hoc must not replace daily app)
#   - candidate-id that does not recompute from candidate.json
#   - HASHES pack_mode that does not match candidate.json
#   - --live extract without Developer ID (real installs; hermetic fixtures skip
#     only when an explicit temp fixture Applications root is set, unless
#     IRIN_LIVE_REQUIRE_DEVELOPER_ID=1)
#   - hermetic --live without IRIN_LIVE_APPLICATIONS_ROOT under a temp root
#     (never resolves to real /Applications under hermetic)
#   - non-canonical candidate.json (identity must match irin_canonical_identity_json)
#   - --live while IRIN.app is running
#   - --live post-swap digest mismatch (restores prior app; no install proof)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }
# shellcheck source=/dev/null
source "$ROOT/packaging/codesign-identity.sh"

# True when path is under an allowed hermetic/temp root (not real /Applications).
path_under_temp_root() {
  local path="$1" tmp_base phys
  tmp_base="${TMPDIR:-/tmp}"
  if [[ -d "$tmp_base" ]]; then
    tmp_base="$(cd "$tmp_base" && pwd -P)" || tmp_base="${TMPDIR:-/tmp}"
  fi
  if [[ -d "$path" ]]; then
    phys="$(cd "$path" && pwd -P)" || return 1
  else
    phys="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$path" 2>/dev/null)" \
      || phys="$path"
  fi
  case "$phys" in
    /tmp/*|/private/tmp/*|"$tmp_base"/*|/var/folders/*)
      return 0
      ;;
  esac
  return 1
}

# Mirror candidate-status.sh hermetic containment (do not edit that file).
# Real candidate stores never honor test overrides.
hermetic_overrides_allowed() {
  [[ "${IRIN_CANDIDATE_STATUS_HERMETIC:-}" == "1" ]] || return 1
  local cand_root
  cand_root="${IRIN_CANDIDATE_ROOT:-}"
  [[ -n "$cand_root" ]] || return 1
  path_under_temp_root "$cand_root"
}

# Hermetic live installs MUST set an explicit fixture Applications root under a
# temp path. Hermetic mode never resolves to real /Applications (PR #77).
resolve_live_applications_root() {
  local override phys
  # Branch on the hermetic flag first: a hermetic run with a missing or
  # non-temp candidate root is an invalid configuration and must refuse —
  # it never falls through to a real /Applications install.
  if [[ "${IRIN_CANDIDATE_STATUS_HERMETIC:-}" == "1" ]] && ! hermetic_overrides_allowed; then
    die "IRIN_CANDIDATE_STATUS_HERMETIC=1 requires IRIN_CANDIDATE_ROOT under a temp root (refusing real /Applications)"
  fi
  if hermetic_overrides_allowed; then
    override="${IRIN_LIVE_APPLICATIONS_ROOT:-}"
    [[ -n "$override" ]] \
      || die "hermetic --live requires IRIN_LIVE_APPLICATIONS_ROOT under a temp root (refusing real /Applications)"
    [[ "$override" == /* ]] || die "IRIN_LIVE_APPLICATIONS_ROOT must be absolute: $override"
    case "$override" in
      /Applications|/Applications/*)
        die "hermetic --live refuses IRIN_LIVE_APPLICATIONS_ROOT under real /Applications: $override"
        ;;
    esac
    mkdir -p "$override" || die "could not create hermetic Applications root: $override"
    phys="$(cd "$override" && pwd -P)" \
      || die "could not resolve hermetic Applications root: $override"
    path_under_temp_root "$phys" \
      || die "hermetic IRIN_LIVE_APPLICATIONS_ROOT must resolve under a temp root (got $phys)"
    printf '%s' "$phys"
    return 0
  fi
  # Real stores / non-hermetic: ignore override entirely.
  printf '%s' "/Applications"
}

resolve_irin_state_root() {
  local override
  override="${IRIN_STATE_ROOT:-}"
  if [[ -n "$override" ]] && hermetic_overrides_allowed; then
    [[ "$override" == /* ]] || die "IRIN_STATE_ROOT must be absolute: $override"
    mkdir -p "$override" || die "could not create hermetic state root: $override"
    printf '%s' "$override"
    return 0
  fi
  printf '%s' "${HOME}/.local/state/irin"
}

irin_app_is_running() {
  local live_app="$1"
  local bin
  # Prefer the live bundle binary path when present.
  bin="$live_app/Contents/MacOS/council-warroom-tauri"
  if [[ -e "$bin" ]] && pgrep -f "$bin" >/dev/null 2>&1; then
    return 0
  fi
  if pgrep -f "${APP_NAME}/Contents/MacOS/" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

# Hermetic fixture Applications root is set and under a temp path (DevID skip ok).
hermetic_fixture_applications_active() {
  hermetic_overrides_allowed || return 1
  [[ -n "${IRIN_LIVE_APPLICATIONS_ROOT:-}" ]] || return 1
  path_under_temp_root "${IRIN_LIVE_APPLICATIONS_ROOT}"
}

# Load helpers only (unit tests). Sourced-only: direct execution must never
# accept an env-driven success-return bypass of the install checks.
if [[ "${IRIN_INSTALL_VERIFY_LIB:-}" == "1" ]]; then
  if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
  fi
  echo "ERROR: IRIN_INSTALL_VERIFY_LIB=1 is only valid when sourced (unit tests)" >&2
  exit 64
fi

CANDIDATE_ARG=""
LIVE_MODE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate)
      CANDIDATE_ARG="${2:-}"
      shift 2
      ;;
    --live)
      LIVE_MODE=1
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Usage: install-verify-candidate.sh --candidate ABSOLUTE_STORE_PATH [--live]

Fresh-mounts the candidate DMG into candidate/install/ (never copies the
sibling stored IRIN.app). Compares candidate vs installed bundle-manifest
digests.

Without --live: writes proofs/install.json for the candidate-local extract.

With --live: after extract verify, staged-swaps the verified extract into
/Applications/IRIN.app (hermetic tests may set IRIN_LIVE_APPLICATIONS_ROOT
only when IRIN_CANDIDATE_STATUS_HERMETIC=1 and the candidate root is a
temp-store path). Refuses pack_mode=local-dev (ad-hoc) and if IRIN.app is
running. On success, writes install proof with live_installed_app_path +
live_installed_bundle_manifest_digest. On post-displacement failure,
restores the prior app and writes no install proof.
EOF
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$CANDIDATE_ARG" ]] || die "usage: $0 --candidate ABSOLUTE_STORE_PATH [--live]"
export IRIN_CANDIDATE_PATH="$CANDIDATE_ARG"
irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"

APP_NAME="IRIN.app"
INSTALL_ROOT="$CANDIDATE/install"
MOUNT="$INSTALL_ROOT/dmg-mount"
DEST_APP="$INSTALL_ROOT/$APP_NAME"
DMG="$(find "$CANDIDATE" -maxdepth 1 -type f -name '*.dmg' | head -1 || true)"
[[ -n "$DMG" && -f "$DMG" ]] || die "candidate DMG missing under $CANDIDATE"

[[ -f "$CANDIDATE/candidate.json" ]] || die "candidate.json missing: $CANDIDATE/candidate.json"
[[ -f "$CANDIDATE/HASHES.txt" ]] || die "HASHES.txt missing: $CANDIDATE/HASHES.txt"

IDENTITY="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' \
  "$CANDIDATE/candidate.json")" \
  || die "could not read candidate.json source_sha"
CANDIDATE_PACK_MODE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("pack_mode",""))' \
  "$CANDIDATE/candidate.json")" \
  || die "could not read candidate.json pack_mode"
CANDIDATE_ID="$(basename "$CANDIDATE")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] || die "candidate path basename is not a candidate-id: $CANDIDATE_ID"

# Candidate-id is sha256 of canonical identity JSON (packaging/env.sh doctrine).
# Refuse non-canonical on-disk serialization even if raw bytes hash to the path.
CANON_TMP="$(mktemp)"
irin_canonical_identity_json <"$CANDIDATE/candidate.json" >"$CANON_TMP" \
  || { rm -f "$CANON_TMP"; die "could not canonicalize candidate.json"; }
if ! cmp -s "$CANDIDATE/candidate.json" "$CANON_TMP"; then
  rm -f "$CANON_TMP"
  die "candidate.json is not in canonical identity form (use irin_canonical_identity_json)"
fi
RECOMPUTED_ID="$(irin_sha256_file "$CANON_TMP")"
rm -f "$CANON_TMP"
[[ "$RECOMPUTED_ID" == "$CANDIDATE_ID" ]] \
  || die "candidate-id does not recompute from candidate.json (store=$CANDIDATE_ID recomputed=$RECOMPUTED_ID)"

HASHES_PACK_MODE="$(awk -F= '$1 == "pack_mode" { print $2; exit }' "$CANDIDATE/HASHES.txt")"
[[ -n "$HASHES_PACK_MODE" ]] || die "HASHES.txt missing pack_mode"
[[ "$HASHES_PACK_MODE" == "$CANDIDATE_PACK_MODE" ]] \
  || die "HASHES pack_mode=$HASHES_PACK_MODE does not match candidate.json pack_mode=$CANDIDATE_PACK_MODE"

CAND_BM="$CANDIDATE/bundle-manifest.txt"
[[ -f "$CAND_BM" ]] || die "bundle-manifest.txt missing: $CAND_BM"
CAND_BM_DIGEST="$(irin_sha256_file "$CAND_BM")"
IDENTITY_BM="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundle_manifest_digest"])' \
  "$CANDIDATE/candidate.json")"
[[ "$CAND_BM_DIGEST" == "$IDENTITY_BM" ]] \
  || die "candidate bundle-manifest digest does not match identity"

# --live: only stable-identity apps may replace the daily installed app.
# local-dev is ad-hoc and re-prompts Keychain on every rebuild; keep it for
# extract/verify/smoke only. pack_mode alone is not enough — see post-extract
# Developer ID proof for non-hermetic live installs.
if [[ "$LIVE_MODE" == "1" ]]; then
  case "$CANDIDATE_PACK_MODE" in
    signed-rc|production) ;;
    local-dev)
      die "refusing --live install of pack_mode=local-dev (ad-hoc); rebuild with signed-rc or production for /Applications"
      ;;
    *)
      die "refusing --live install: pack_mode must be signed-rc or production (got ${CANDIDATE_PACK_MODE:-empty})"
      ;;
  esac
  # Do not leave a partial install proof if live swap fails later.
  rm -f "$CANDIDATE/proofs/install.json"
fi

note "fresh-extract DMG into install/ (not the stored app, not /Applications)"
if mount | grep -q "$MOUNT"; then
  hdiutil detach "$MOUNT" -force 2>/dev/null || true
fi
# Keep proofs/ untouched; wipe only install disposable root contents.
rm -rf "$DEST_APP" "$MOUNT" "$INSTALL_ROOT/bundle-manifest.txt"
mkdir -p "$INSTALL_ROOT" "$MOUNT"
hdiutil attach "$DMG" -mountpoint "$MOUNT" -readonly -nobrowse
trap 'hdiutil detach "$MOUNT" -force 2>/dev/null || true' EXIT
SRC_APP="$(find "$MOUNT" -maxdepth 2 -name "$APP_NAME" -type d | head -1 || true)"
[[ -d "$SRC_APP" ]] || die "app not found in DMG"
ditto "$SRC_APP" "$DEST_APP"
hdiutil detach "$MOUNT" -force 2>/dev/null || true
trap - EXIT
rm -rf "$MOUNT"
[[ -d "$DEST_APP" ]] || die "missing app after extract: $DEST_APP"

note "recompute installed bundle-manifest"
irin_write_bundle_manifest "$DEST_APP" "$INSTALL_ROOT/bundle-manifest.txt"
INST_BM_DIGEST="$(irin_sha256_file "$INSTALL_ROOT/bundle-manifest.txt")"

[[ "$INST_BM_DIGEST" == "$CAND_BM_DIGEST" ]] \
  || die "installed bundle-manifest digest diverges from candidate (installed=$INST_BM_DIGEST candidate=$CAND_BM_DIGEST)"

# --live requires a real Developer ID code signature on the outer app and the
# same nested Mach-O identity binding as packaging/verify-dmg.sh (TeamIdentifier
# + per-binary DevID). Hermetic fixture destinations (temp Applications root)
# may skip unless IRIN_LIVE_REQUIRE_DEVELOPER_ID=1.
if [[ "$LIVE_MODE" == "1" ]]; then
  require_dev_id=1
  if hermetic_fixture_applications_active \
    && [[ "${IRIN_LIVE_REQUIRE_DEVELOPER_ID:-}" != "1" ]]; then
    require_dev_id=0
  fi
  if [[ "$require_dev_id" == "1" ]]; then
    note "prove Developer ID + nested Mach-O identity on extract"
    codesign --verify --deep --strict "$DEST_APP" \
      || die "refusing --live: codesign verification failed on extract (not Developer ID-stable)"
    irin_assert_nested_developer_id_identity "$DEST_APP" \
      || die "refusing --live: nested Mach-O Developer ID / TeamIdentifier binding failed"
  fi
fi

# Content-identity equality (path/kind/payload + freeze-normalized mode) is
# enforced by candidate-status; we still refuse obvious path/kind/payload diffs.
python3 - "$CAND_BM" "$INSTALL_ROOT/bundle-manifest.txt" <<'PY' || die "install vs candidate bundle-manifest content identity diverges"
import sys
cand = open(sys.argv[1], encoding="utf-8").read().splitlines()
inst = open(sys.argv[2], encoding="utf-8").read().splitlines()

def content_rows(lines):
    out = {}
    for line in lines:
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) < 4:
            raise SystemExit(f"bad manifest line: {line!r}")
        path, kind, mode, payload = parts[0], parts[1], parts[2], parts[3]
        # Freeze-norm: clear write bits for comparison.
        try:
            m = int(mode, 8) & ~0o222
            mode_n = format(m, "04o")
        except ValueError:
            mode_n = mode
        out[path] = (kind, mode_n, payload)
    return out

if content_rows(cand) != content_rows(inst):
    raise SystemExit(1)
PY

LIVE_APP_PATH=""
LIVE_BM_DIGEST=""

# Live rollback globals (EXIT trap must see them).
LIVE_ROLLBACK_ARMED=0
SAVED_PRIOR=""
STAGING=""
LIVE_APP=""
APPS_ROOT=""
DISPLACE_ROOT=""
DISPLACED=0

disarm_live_rollback() {
  LIVE_ROLLBACK_ARMED=0
  trap - EXIT
}

make_tree_removable() {
  local path="$1"
  [[ -e "$path" || -L "$path" ]] || return 0
  chmod -R u+w "$path"
}

# Never delete SAVED_PRIOR (sibling or archive). On restore failure leave it and hard-error.
live_rollback() {
  local msg="$1"
  LIVE_ROLLBACK_ARMED=0
  trap - EXIT

  # Guard empty paths — never rm -rf "" or proofs under empty CANDIDATE.
  if [[ -n "${STAGING:-}" ]]; then
    make_tree_removable "$STAGING" 2>/dev/null || true
    rm -rf "$STAGING" 2>/dev/null || true
  fi
  if [[ -n "${CANDIDATE:-}" ]]; then
    rm -f "$CANDIDATE/proofs/install.json" 2>/dev/null || true
  fi

  if [[ -n "${SAVED_PRIOR:-}" ]]; then
    if [[ ! -e "$SAVED_PRIOR" && ! -L "$SAVED_PRIOR" ]]; then
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: saved prior missing at %s; cannot restore\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    if [[ -z "${LIVE_APP:-}" ]]; then
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: LIVE_APP unset; saved prior left at %s (not deleted)\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    if [[ "$SAVED_PRIOR" == "$LIVE_APP" ]]; then
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: SAVED_PRIOR equals LIVE_APP (%s); refusing nested restore\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    # /Applications is sunlnk on macOS: retain an immutable failed replacement
    # under a hidden same-directory name instead of trying to unlink it.
    if [[ -e "$LIVE_APP" || -L "$LIVE_APP" ]]; then
      local failed_live="$APPS_ROOT/.${APP_NAME}.irin-failed.$$"
      if [[ -e "$failed_live" || -L "$failed_live" ]]; then
        printf 'ERROR: %s\n' "$msg" >&2
        printf 'ERROR: failed-app recovery collision at %s; saved prior left at %s\n' "$failed_live" "$SAVED_PRIOR" >&2
        exit 1
      fi
      if ! mv "$LIVE_APP" "$failed_live"; then
        printf 'ERROR: %s\n' "$msg" >&2
        printf 'ERROR: could not retain failed replacement at %s; saved prior left at %s\n' "$failed_live" "$SAVED_PRIOR" >&2
        exit 1
      fi
      note "retained failed replacement at $failed_live"
    fi
    if ! ditto "$SAVED_PRIOR" "$LIVE_APP"; then
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: failed to restore prior app from %s; retained recovery copy\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    local saved_bm live_bm saved_digest live_digest
    saved_bm="$(mktemp)"
    live_bm="$(mktemp)"
    if ! irin_write_bundle_manifest "$SAVED_PRIOR" "$saved_bm" \
      || ! irin_write_bundle_manifest "$LIVE_APP" "$live_bm"; then
      rm -f "$saved_bm" "$live_bm"
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: could not verify restored prior; recovery copy retained at %s\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    saved_digest="$(irin_sha256_file "$saved_bm")"
    live_digest="$(irin_sha256_file "$live_bm")"
    rm -f "$saved_bm" "$live_bm"
    if [[ "$saved_digest" != "$live_digest" ]]; then
      printf 'ERROR: %s\n' "$msg" >&2
      printf 'ERROR: restored prior digest mismatch; recovery copy retained at %s\n' "$SAVED_PRIOR" >&2
      exit 1
    fi
    note "restored prior app to $LIVE_APP (recovery copy retained at $SAVED_PRIOR)"
  else
    if [[ -n "${LIVE_APP:-}" && ( -e "$LIVE_APP" || -L "$LIVE_APP" ) ]]; then
      local failed_live="$APPS_ROOT/.${APP_NAME}.irin-failed.$$"
      if [[ ! -e "$failed_live" && ! -L "$failed_live" ]]; then
        mv "$LIVE_APP" "$failed_live" 2>/dev/null || true
      fi
    fi
  fi

  printf 'ERROR: %s\n' "$msg" >&2
  exit 1
}

live_rollback_on_exit() {
  [[ "${LIVE_ROLLBACK_ARMED:-0}" == "1" ]] || return 0
  live_rollback "live install aborted after displacement (EXIT before durable install proof)"
}

arm_live_rollback() {
  LIVE_ROLLBACK_ARMED=1
  trap 'live_rollback_on_exit' EXIT
}

if [[ "$LIVE_MODE" == "1" ]]; then
  note "--live: staged swap into Applications"
  APPS_ROOT="$(resolve_live_applications_root)"
  LIVE_APP="$APPS_ROOT/$APP_NAME"
  STATE_ROOT="$(resolve_irin_state_root)"
  DISPLACE_ROOT="$STATE_ROOT/displaced-apps"
  STAGING="$APPS_ROOT/${APP_NAME}.irin-staging.$$"
  PRIOR="$APPS_ROOT/${APP_NAME}.irin-prior.$$"
  SAVED_PRIOR=""
  DISPLACED=0

  if irin_app_is_running "$LIVE_APP"; then
    die "IRIN.app is running; quit it before --live install"
  fi

  mkdir -p "$APPS_ROOT" || die "could not create Applications root: $APPS_ROOT"

  # Clean leftover staging only. NEVER rm PRIOR — a PID-reused stale prior is
  # operator recovery data; refuse and leave it untouched.
  if [[ -n "$STAGING" ]]; then
    rm -rf "$STAGING"
  fi
  if [[ -e "$PRIOR" || -L "$PRIOR" ]]; then
    die "stale PID-scoped prior exists at $PRIOR; left untouched. Recover: inspect that bundle, move it aside only if intentional, then retry --live install"
  fi

  note "stage faithful ditto copy as sibling on destination filesystem"
  ditto "$DEST_APP" "$STAGING" || die "ditto stage failed"

  # Pre-swap stage digest (fail before displacement when possible).
  STAGE_BM="$(mktemp)"
  irin_write_bundle_manifest "$STAGING" "$STAGE_BM"
  STAGE_DIGEST="$(irin_sha256_file "$STAGE_BM")"
  rm -f "$STAGE_BM"
  [[ "$STAGE_DIGEST" == "$CAND_BM_DIGEST" ]] \
    || { [[ -n "$STAGING" ]] && rm -rf "$STAGING"; die "staged app digest diverges from candidate before swap"; }

  if [[ -e "$LIVE_APP" || -L "$LIVE_APP" ]]; then
    note "displace existing live app aside"
    mv "$LIVE_APP" "$PRIOR" || { [[ -n "$STAGING" ]] && rm -rf "$STAGING"; die "could not displace existing $LIVE_APP"; }
    DISPLACED=1
    SAVED_PRIOR="$PRIOR"
    # Arm for every EXIT until install proof is durable.
    arm_live_rollback
  fi

  note "move staging into place"
  if ! mv "$STAGING" "$LIVE_APP"; then
    if [[ "$DISPLACED" == "1" ]]; then
      live_rollback "could not move staged app into $LIVE_APP"
    fi
    [[ -n "$STAGING" ]] && rm -rf "$STAGING"
    die "could not move staged app into $LIVE_APP"
  fi
  STAGING=""

  # No prior: still arm once live path holds the new app (proof may fail later).
  if [[ "$LIVE_ROLLBACK_ARMED" != "1" ]]; then
    arm_live_rollback
  fi

  note "recompute live bundle-manifest at final path"
  LIVE_BM="$(mktemp)"
  if ! irin_write_bundle_manifest "$LIVE_APP" "$LIVE_BM"; then
    rm -f "$LIVE_BM"
    live_rollback "could not write live bundle-manifest"
  fi
  LIVE_BM_DIGEST="$(irin_sha256_file "$LIVE_BM")"
  rm -f "$LIVE_BM"
  if [[ "$LIVE_BM_DIGEST" != "$CAND_BM_DIGEST" ]]; then
    live_rollback \
      "live install digest mismatch after swap (live=$LIVE_BM_DIGEST candidate=$CAND_BM_DIGEST)"
  fi

  # Archive displaced old app under state root (never delete; never leave in Applications).
  if [[ "$DISPLACED" == "1" && -n "$SAVED_PRIOR" && ( -e "$SAVED_PRIOR" || -L "$SAVED_PRIOR" ) ]]; then
    mkdir -p "$DISPLACE_ROOT" || live_rollback "could not create displaced-apps root"
    # Physical containment: resolve both sides with pwd -P so a symlink-rooted
    # displaced-apps (or ancestor) into the newly installed app cannot archive
    # inside the live bundle after its digest was already verified.
    DISPLACE_PHYS="$(cd "$DISPLACE_ROOT" && pwd -P)" \
      || live_rollback "could not resolve physical displaced-apps path: $DISPLACE_ROOT"
    if [[ -d "$LIVE_APP" && ! -L "$LIVE_APP" ]]; then
      LIVE_PHYS="$(cd "$LIVE_APP" && pwd -P)" \
        || live_rollback "could not resolve physical live app path: $LIVE_APP"
    else
      LIVE_PHYS="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$LIVE_APP")" \
        || live_rollback "could not resolve physical live app path: $LIVE_APP"
    fi
    case "$DISPLACE_PHYS" in
      "$LIVE_PHYS"|"$LIVE_PHYS"/*)
        live_rollback "refusing displaced-apps nest under live app path: $DISPLACE_ROOT (phys $DISPLACE_PHYS under $LIVE_PHYS)"
        ;;
    esac
    TS="$(date -u +%Y%m%dT%H%M%SZ)"
    # Collision-safe: timestamp + candidate prefix + PID.
    ARCHIVE="$DISPLACE_ROOT/${APP_NAME}.${TS}.${CANDIDATE_ID:0:12}.$$"
    if [[ -e "$ARCHIVE" || -L "$ARCHIVE" ]]; then
      live_rollback "archive destination collision (refusing overwrite): $ARCHIVE"
    fi
    # /Applications and the state root may be different filesystems. Copy the
    # prior bundle faithfully, prove the copy, then remove the sibling prior.
    PRIOR_BM="$(mktemp)"
    ARCHIVE_BM="$(mktemp)"
    if ! ditto "$SAVED_PRIOR" "$ARCHIVE"; then
      rm -f "$PRIOR_BM" "$ARCHIVE_BM"
      make_tree_removable "$ARCHIVE" 2>/dev/null || true
      rm -rf "$ARCHIVE"
      live_rollback "could not archive displaced prior app"
    fi
    if ! irin_write_bundle_manifest "$SAVED_PRIOR" "$PRIOR_BM" \
      || ! irin_write_bundle_manifest "$ARCHIVE" "$ARCHIVE_BM"; then
      rm -f "$PRIOR_BM" "$ARCHIVE_BM"
      make_tree_removable "$ARCHIVE" 2>/dev/null || true
      rm -rf "$ARCHIVE"
      live_rollback "could not verify archived prior app"
    fi
    PRIOR_DIGEST="$(irin_sha256_file "$PRIOR_BM")"
    ARCHIVE_DIGEST="$(irin_sha256_file "$ARCHIVE_BM")"
    rm -f "$PRIOR_BM" "$ARCHIVE_BM"
    if [[ "$PRIOR_DIGEST" != "$ARCHIVE_DIGEST" ]]; then
      make_tree_removable "$ARCHIVE" 2>/dev/null || true
      rm -rf "$ARCHIVE"
      live_rollback "archived prior app digest mismatch"
    fi
    # Rollback now restores from the proven archive. /Applications is sunlnk,
    # so retain the immutable prior under a hidden same-directory name.
    SAVED_PRIOR="$ARCHIVE"
    HIDDEN_PRIOR="$APPS_ROOT/.${APP_NAME}.irin-prior.${TS}.${CANDIDATE_ID:0:12}.$$"
    if [[ -e "$HIDDEN_PRIOR" || -L "$HIDDEN_PRIOR" ]]; then
      live_rollback "hidden prior recovery collision: $HIDDEN_PRIOR"
    fi
    if ! mv "$PRIOR" "$HIDDEN_PRIOR"; then
      live_rollback "could not retain displaced prior under hidden recovery name"
    fi
    note "archived prior app: $ARCHIVE"
    note "retained hidden prior app: $HIDDEN_PRIOR"
  fi

  LIVE_APP_PATH="$(cd "$LIVE_APP" && pwd)"
  note "live install complete (proof not yet durable): $LIVE_APP_PATH"
fi

# Paths/digests via env — never interpolate into an unquoted Python string.
EXTRA="$(
  CAND_BM_DIGEST="$CAND_BM_DIGEST" \
  INST_BM_DIGEST="$INST_BM_DIGEST" \
  DEST_APP="$DEST_APP" \
  DMG="$DMG" \
  LIVE_MODE="$LIVE_MODE" \
  LIVE_APP_PATH="$LIVE_APP_PATH" \
  LIVE_BM_DIGEST="$LIVE_BM_DIGEST" \
  python3 - <<'PY'
import json, os
extra = {
  "candidate_bundle_manifest_digest": os.environ["CAND_BM_DIGEST"],
  "installed_bundle_manifest_digest": os.environ["INST_BM_DIGEST"],
  "installed_app_path": os.environ["DEST_APP"],
  "dmg_path": os.environ["DMG"],
}
if os.environ.get("LIVE_MODE") == "1":
    live_path = os.environ.get("LIVE_APP_PATH") or ""
    live_digest = os.environ.get("LIVE_BM_DIGEST") or ""
    if not live_path or not live_digest:
        raise SystemExit("live fields missing after --live success")
    extra["live_installed_app_path"] = live_path
    extra["live_installed_bundle_manifest_digest"] = live_digest
print(json.dumps(extra))
PY
)"

# Under --live, rollback stays armed until install proof is durable on disk.
irin_write_proof_envelope \
  "$CANDIDATE/proofs/install.json" \
  "install" \
  "$CANDIDATE_ID" \
  "$IDENTITY" \
  "PASS" \
  "$EXTRA"

if [[ ! -f "$CANDIDATE/proofs/install.json" ]]; then
  if [[ "$LIVE_MODE" == "1" && "$LIVE_ROLLBACK_ARMED" == "1" ]]; then
    live_rollback "install proof missing after write attempt"
  fi
  die "install proof missing after write attempt"
fi

if [[ "$LIVE_MODE" == "1" ]]; then
  # Proof is durable — only now disarm post-displacement rollback.
  disarm_live_rollback
fi

note "install proof written"
echo "candidate_path=$CANDIDATE"
echo "install_app=$DEST_APP"
echo "candidate_bundle_manifest_digest=$CAND_BM_DIGEST"
echo "installed_bundle_manifest_digest=$INST_BM_DIGEST"
if [[ "$LIVE_MODE" == "1" ]]; then
  echo "live_installed_app_path=$LIVE_APP_PATH"
  echo "live_installed_bundle_manifest_digest=$LIVE_BM_DIGEST"
fi
echo "proof=$CANDIDATE/proofs/install.json"
