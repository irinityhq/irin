#!/usr/bin/env bash
# IRIN release transaction — two non-overlapping actions.
#
#   --prepare-production --t1-packet PATH
#       T1-authorized RC preparation: preflight, push rc-* images (once per
#       attempt), resolve digests, one production notarized DMG into the
#       candidate store, verify + smoke. Does NOT tag or touch a GitHub Release.
#       Retry under the same unexpired T1 reuses matching completed effects only.
#
#   --publish --tag vX.Y.Z --candidate ABSOLUTE_STORE_PATH \
#             --t2-packet CANDIDATE/proofs/t2.json
#       Publication only. Never rebuilds. Order: remote-tag check → release
#       draft/public state → labels (create only for draft path) → tag → attach
#       → publish → unauthenticated re-download → proofs/publication.json.
#
# The misleading --dry-run-rc name is removed. Use --prepare-production only;
# that path has irreversible GHCR/notary effects and is not a no-effect simulation.
#
# Publication never installs. On first publication only (publication proof
# absent), before mutation, recompute /Applications/IRIN.app manifest equality
# to the candidate and refuse mismatch. Skipped under publish_hermetic_active
# and on already-published idempotent validation/retry.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

# Mirror candidate-status.sh hermetic containment (do not edit that file).
hermetic_overrides_allowed() {
  [[ "${IRIN_CANDIDATE_STATUS_HERMETIC:-}" == "1" ]] || return 1
  local tmp_base cand_root
  tmp_base="${TMPDIR:-/tmp}"
  if [[ -d "$tmp_base" ]]; then
    tmp_base="$(cd "$tmp_base" && pwd -P)" || tmp_base="${TMPDIR:-/tmp}"
  fi
  cand_root="${IRIN_CANDIDATE_ROOT:-}"
  [[ -n "$cand_root" ]] || return 1
  if [[ -d "$cand_root" ]]; then
    cand_root="$(cd "$cand_root" && pwd -P)" || return 1
  fi
  case "$cand_root" in
    /tmp/*|/private/tmp/*|"$tmp_base"/*|/var/folders/*)
      return 0
      ;;
  esac
  return 1
}

resolve_live_applications_root() {
  local override
  override="${IRIN_LIVE_APPLICATIONS_ROOT:-}"
  if [[ -n "$override" ]] && hermetic_overrides_allowed; then
    [[ "$override" == /* ]] || die "IRIN_LIVE_APPLICATIONS_ROOT must be absolute: $override"
    printf '%s' "$override"
    return 0
  fi
  printf '%s' "/Applications"
}

# Point-in-time first-publish gate: live daily-use app must match candidate.
# Not a standing derived condition. Callers skip when publication.json exists
# or publish_hermetic_active is set.
require_live_app_matches_candidate() {
  local candidate="$1"
  local cand_bm_digest live_root live_app tmp_bm live_digest
  cand_bm_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundle_manifest_digest"])' \
    "$candidate/candidate.json")" \
    || die "could not read candidate bundle_manifest_digest"
  live_root="$(resolve_live_applications_root)"
  live_app="$live_root/IRIN.app"
  [[ -d "$live_app" ]] \
    || die "first publish requires live app at $live_app matching candidate (missing)"
  tmp_bm="$(mktemp)"
  irin_write_bundle_manifest "$live_app" "$tmp_bm" \
    || { rm -f "$tmp_bm"; die "could not recompute live app bundle-manifest"; }
  live_digest="$(irin_sha256_file "$tmp_bm")"
  rm -f "$tmp_bm"
  [[ "$live_digest" == "$cand_bm_digest" ]] \
    || die "first publish refuses live app digest mismatch (live=$live_digest candidate=$cand_bm_digest at $live_app)"
  note "first-publish live app digest matches candidate at $live_app"
}

MODE=""
TAG=""
CANDIDATE_ARG=""
T1_PACKET=""
T2_PACKET=""
T3_EXCEPTION=""

usage() {
  cat <<'EOF'
Usage:
  release-transaction.sh --prepare-production --t1-packet PATH \
      [--t3-exception PATH]
  release-transaction.sh --publish --tag vX.Y.Z \
      --candidate ABSOLUTE_STORE_PATH --t2-packet CANDIDATE/proofs/t2.json

  --prepare-production is T1-authorized RC preparation with irreversible
  external effects (rc-* GHCR push, Apple notary once per attempt). It is not
  a no-effect simulation. There is no --dry-run-rc alias.

  One production notarization consumes the T1 production cycle for that source
  SHA. A second prepare for the same SHA requires --t3-exception PATH (words
  must name the SHA and mention apple). Prepare records checkout HEAD and
  whether scripts/ or packaging/ is dirty; publish requires that same HEAD and
  a clean scripts/+packaging/ tree.

Required env (both modes):
  APPLE_SIGNING_IDENTITY   Developer ID Application identity
  APPLE_NOTARY_PROFILE     notarytool keychain profile

Hermetic tests may source this file with IRIN_RELEASE_TX_LIB=1 to load helpers
only (no mode dispatch).

Publish hermetic rehearsal (W5, zero network) — dual gate required:
  IRIN_PUBLISH_HERMETIC=1
  IRIN_PUBLISH_HERMETIC_CONFIRM=shipping-method-smoke
    - both must match exactly; either alone is ignored / refused
    - remote tag peel uses IRIN_PUBLISH_REMOTE_TAG_SHA (empty = absent)
    - skips git tag create/push (no local mutation, no network)
    - skips docker login (fake docker/gh on PATH supply GHCR + release I/O)
  IRIN_RELEASE_DRAFT_WAIT_ATTEMPTS / IRIN_RELEASE_DRAFT_WAIT_SLEEP
    - bound the draft-release poll (defaults 30 / 2s)
EOF
}

# Hermetic publish is test-only. Require a deliberate confirm string so an
# inherited IRIN_PUBLISH_HERMETIC=1 cannot skip live tag/GHCR safeguards.
publish_hermetic_active() {
  if [[ "${IRIN_PUBLISH_HERMETIC:-}" != "1" ]]; then
    return 1
  fi
  if [[ "${IRIN_PUBLISH_HERMETIC_CONFIRM:-}" != "shipping-method-smoke" ]]; then
    die "IRIN_PUBLISH_HERMETIC=1 requires IRIN_PUBLISH_HERMETIC_CONFIRM=shipping-method-smoke (test-only dual gate)"
  fi
  return 0
}

# Skip CLI parse/dispatch when sourced as a library for tests.
if [[ "${IRIN_RELEASE_TX_LIB:-}" != "1" ]]; then
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --prepare-production) MODE="prepare"; shift ;;
      --dry-run-rc)
        die "--dry-run-rc was removed; use --prepare-production --t1-packet PATH (irreversible; not a dry run)"
        ;;
      --publish) MODE="publish"; shift ;;
      --tag) TAG="${2:-}"; shift 2 ;;
      --candidate) CANDIDATE_ARG="${2:-}"; shift 2 ;;
      --t1-packet) T1_PACKET="${2:-}"; shift 2 ;;
      --t2-packet) T2_PACKET="${2:-}"; shift 2 ;;
      --t3-exception) T3_EXCEPTION="${2:-}"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown argument: $1 (try --help)" ;;
    esac
  done
  [[ -n "$MODE" ]] || { usage >&2; die "mode required"; }
fi

# ---------------------------------------------------------------------------
# Checkout control-plane binding (HEAD + scripts/packaging dirty)
# ---------------------------------------------------------------------------
# Sets CHECKOUT_HEAD, SCRIPTS_DIRTY, PACKAGING_DIRTY (true/false strings).
# Do not parse porcelain path display (C-quoting breaks on spaces); query each
# pathspec and treat any non-empty porcelain as dirty. Fail closed on git error.
snapshot_checkout_control() {
  CHECKOUT_HEAD="$(git rev-parse HEAD)"
  SCRIPTS_DIRTY=false
  PACKAGING_DIRTY=false
  local status_out
  if ! status_out="$(git status --porcelain --untracked-files=normal -- scripts 2>&1)"; then
    die "git status failed for scripts/: $status_out"
  fi
  [[ -n "$status_out" ]] && SCRIPTS_DIRTY=true
  if ! status_out="$(git status --porcelain --untracked-files=normal -- packaging 2>&1)"; then
    die "git status failed for packaging/: $status_out"
  fi
  [[ -n "$status_out" ]] && PACKAGING_DIRTY=true
}

# ---------------------------------------------------------------------------
# Production-cycle ledger: notarization spends one cycle per source SHA
# ---------------------------------------------------------------------------
# States: missing | reserved | consumed. Invalid ledger always aborts.
# All mutations take an exclusive flock on production-cycle-<sha>.json.lock
# and compare-and-swap expected status. Each T3 is single-use by packet digest
# (global .attempts/t3-spent/<digest>.json + per-source spent_t3_digests).
production_cycle_path() {
  local sha="$1"
  printf '%s/.attempts/production-cycle-%s.json\n' "$IRIN_CANDIDATE_ROOT" "$sha"
}

t3_spent_path() {
  local digest="$1"
  printf '%s/.attempts/t3-spent/%s.json\n' "$IRIN_CANDIDATE_ROOT" "$digest"
}

# Prints: missing | reserved | consumed
# Dies if the ledger file exists but is malformed / wrong SHA.
production_cycle_state() {
  local sha="$1" path
  path="$(production_cycle_path "$sha")"
  if [[ ! -f "$path" ]]; then
    printf 'missing\n'
    return 0
  fi
  python3 - "$path" "$sha" <<'PY' || die "production-cycle ledger invalid for $sha (see stderr)"
import json, sys
path, sha = sys.argv[1:]
try:
    d = json.load(open(path))
except Exception as e:
    print(f"ledger unreadable: {e}", file=sys.stderr)
    sys.exit(1)
if d.get("kind") not in (None, "production-cycle"):
    print(f"ledger kind invalid: {d.get('kind')!r}", file=sys.stderr)
    sys.exit(1)
if d.get("source_sha") != sha:
    print(
        f"ledger source_sha mismatch: {d.get('source_sha')!r} != {sha!r}",
        file=sys.stderr,
    )
    sys.exit(1)
status = d.get("status")
if status in ("reserved", "consumed"):
    print(status)
    sys.exit(0)
if d.get("notarization_consumed") is True:
    print("consumed")
    sys.exit(0)
print(f"ledger status invalid: {status!r}", file=sys.stderr)
sys.exit(1)
PY
}

# Returns 0 if state is consumed (or reserved — budget already claimed).
production_cycle_consumed() {
  local sha="$1" state
  state="$(production_cycle_state "$sha")"
  [[ "$state" == "consumed" || "$state" == "reserved" ]]
}

# Validate T3 packet shape. Prints sha256 digest of packet bytes on success.
# Does not record spend — reserve_production_cycle claims the digest under lock.
validate_t3_exception() {
  local path="$1" want_sha="$2"
  [[ -n "$path" ]] || die "--t3-exception PATH is required for a second production cycle"
  [[ "$path" == /* ]] || path="$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
  [[ -f "$path" ]] || die "T3 exception missing: $path"
  python3 - "$path" "$want_sha" <<'PY'
import hashlib, json, re, sys
path, want_sha = sys.argv[1:]
raw = open(path, "rb").read()
d = json.loads(raw.decode("utf-8"))
if d.get("schema_version") != 1:
    raise SystemExit("T3 schema_version must be 1")
if d.get("packet_kind") != "t3":
    raise SystemExit("T3 packet_kind must be 't3'")
sha = d.get("source_sha")
if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
    raise SystemExit("T3 source_sha must be 40-char full git SHA (hex)")
if sha.lower() != want_sha.lower():
    raise SystemExit(f"T3 source_sha {sha} does not match prepare source {want_sha}")
words = d.get("words")
if not isinstance(words, str) or not words.strip():
    raise SystemExit("T3 words must be a non-empty string (must name the SHA and mention apple)")
low = words.lower()
if "apple" not in low:
    raise SystemExit("T3 words must mention apple (Apple notary cycle override)")
if want_sha.lower() not in low and sha.lower() not in low:
    raise SystemExit("T3 words must name the source SHA")
print(hashlib.sha256(raw).hexdigest())
PY
}

# Exclusive claim before first external effect.
# Args: sha attempt [t3_path] checkout_head scripts_dirty packaging_dirty
# - missing → reserved (O_EXCL under flock)
# - reserved same attempt → ok
# - reserved foreign / consumed → require unused T3 digest; CAS under flock
# - binds T1 checkout fields onto the cycle ledger for publish
reserve_production_cycle() {
  local sha="$1" attempt="$2" t3_path="${3:-}" head="$4" scripts_dirty="$5" packaging_dirty="$6"
  local path t3_digest=""
  path="$(production_cycle_path "$sha")"
  mkdir -p "$(dirname "$path")" "$(dirname "$(t3_spent_path x)")"
  if [[ -n "$t3_path" ]]; then
    [[ "$t3_path" == /* ]] || t3_path="$(cd "$(dirname "$t3_path")" && pwd)/$(basename "$t3_path")"
    t3_digest="$(validate_t3_exception "$t3_path" "$sha")" \
      || die "T3 exception invalid for $sha"
  fi
  python3 - "$path" "$sha" "$attempt" "$t3_path" "$t3_digest" \
    "$head" "$scripts_dirty" "$packaging_dirty" "$IRIN_CANDIDATE_ROOT" \
    <<'PY' || die "cannot reserve production cycle for $sha"
import fcntl, hashlib, json, os, sys
from datetime import datetime, timezone

(
    out,
    sha,
    attempt,
    t3_path,
    t3_digest,
    checkout_head,
    scripts_dirty,
    packaging_dirty,
    cand_root,
) = sys.argv[1:]
now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
lock_path = out + ".lock"
spent_dir = os.path.join(cand_root, ".attempts", "t3-spent")
os.makedirs(os.path.dirname(out), exist_ok=True)
os.makedirs(spent_dir, exist_ok=True)

def spent_path(digest: str) -> str:
    return os.path.join(spent_dir, f"{digest}.json")

def normalize_status(d):
    status = d.get("status")
    if status in ("reserved", "consumed"):
        return status
    if d.get("notarization_consumed") is True:
        return "consumed"
    return status

def atomic_write(path, doc):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, sort_keys=True, indent=2)
        fh.write("\n")
    os.replace(tmp, path)

def claim_t3(digest, source_sha, attempt_id):
    if not digest:
        raise SystemExit("T3 digest missing for cycle override")
    sp = spent_path(digest)
    if os.path.exists(sp):
        raise SystemExit(
            f"T3 packet digest {digest} already spent; authorize a new T3 exception"
        )
    # O_EXCL global spend record (single-use identity).
    payload = json.dumps(
        {
            "schema_version": 1,
            "kind": "t3-spent",
            "t3_packet_sha256": digest,
            "source_sha": source_sha,
            "production_attempt_id": attempt_id,
            "spent_at": now,
        },
        sort_keys=True,
        indent=2,
    ) + "\n"
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
    try:
        fd = os.open(sp, flags, 0o644)
    except FileExistsError:
        raise SystemExit(
            f"T3 packet digest {digest} already spent (race); authorize a new T3"
        )
    try:
        os.write(fd, payload.encode("utf-8"))
    finally:
        os.close(fd)

def base_reserved_doc(prev, t3_digest=None):
    spent = list((prev or {}).get("spent_t3_digests") or [])
    if t3_digest and t3_digest not in spent:
        spent.append(t3_digest)
    return {
        "schema_version": 1,
        "kind": "production-cycle",
        "source_sha": sha,
        "status": "reserved",
        "notarization_consumed": False,
        "production_attempt_id": attempt,
        "reserved_at": now,
        "checkout_head": checkout_head,
        "scripts_dirty": scripts_dirty == "true",
        "packaging_dirty": packaging_dirty == "true",
        "t3_exception_path": t3_path or None,
        "t3_packet_sha256": t3_digest or None,
        "spent_t3_digests": spent,
        "prior_consumed_attempt_id": (prev or {}).get("production_attempt_id")
        if (prev or {}).get("status") == "consumed"
        or (prev or {}).get("notarization_consumed") is True
        else (prev or {}).get("prior_consumed_attempt_id"),
        "prior_reserved_attempt_id": (prev or {}).get("production_attempt_id")
        if (prev or {}).get("status") == "reserved"
        and (prev or {}).get("production_attempt_id") != attempt
        else (prev or {}).get("prior_reserved_attempt_id"),
    }

with open(lock_path, "a+", encoding="utf-8") as lockf:
    fcntl.flock(lockf.fileno(), fcntl.LOCK_EX)
    if not os.path.exists(out):
        # First cycle: no T3 required; exclusive create under lock.
        flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
        doc = base_reserved_doc(None, None)
        payload = json.dumps(doc, sort_keys=True, indent=2) + "\n"
        try:
            fd = os.open(out, flags, 0o644)
        except FileExistsError:
            raise SystemExit("race: cycle ledger appeared during reserve")
        try:
            os.write(fd, payload.encode("utf-8"))
        finally:
            os.close(fd)
        print("reserved_new")
        sys.exit(0)

    try:
        d = json.load(open(out))
    except Exception as e:
        raise SystemExit(f"ledger unreadable: {e}")
    if d.get("source_sha") != sha:
        raise SystemExit(f"ledger source_sha mismatch: {d.get('source_sha')!r}")
    status = normalize_status(d)
    if status == "reserved" and d.get("production_attempt_id") == attempt:
        # Same-attempt resume; refuse mismatched checkout binding.
        if d.get("checkout_head") not in (None, checkout_head):
            raise SystemExit(
                f"reserved cycle checkout_head {d.get('checkout_head')!r} != {checkout_head!r}"
            )
        print("reserved_same_attempt")
        sys.exit(0)

    if status == "reserved" and d.get("production_attempt_id") != attempt:
        if not t3_digest:
            raise SystemExit(
                f"production cycle reserved by attempt "
                f"{d.get('production_attempt_id')!r}; provide --t3-exception "
                f"to recover an abandoned reservation"
            )
    elif status == "consumed":
        if not t3_digest:
            raise SystemExit(
                "production notarization already consumed; "
                "authorize a T3 exception naming that SHA (--t3-exception PATH)"
            )
    else:
        raise SystemExit(f"ledger status invalid: {status!r}")

    # T3 path: single-use digest + CAS reserved write under flock.
    if t3_digest in (d.get("spent_t3_digests") or []):
        raise SystemExit(
            f"T3 packet digest {t3_digest} already spent for source {sha}"
        )
    claim_t3(t3_digest, sha, attempt)
    d2 = json.load(open(out))
    st2 = normalize_status(d2)
    if st2 not in ("consumed", "reserved"):
        raise SystemExit(f"CAS failed: unexpected status {st2!r}")
    if st2 == "reserved" and d2.get("production_attempt_id") == attempt:
        print("reserved_same_attempt")
        sys.exit(0)
    # Expected pre-state for this transition: still reserved-by-other or consumed.
    if st2 == "reserved" and d2.get("production_attempt_id") != d.get("production_attempt_id"):
        raise SystemExit(
            f"CAS failed: reservation holder changed "
            f"{d.get('production_attempt_id')!r} -> {d2.get('production_attempt_id')!r}"
        )
    if st2 == "consumed" and status != "consumed":
        raise SystemExit("CAS failed: status flipped to consumed during T3 recover")
    atomic_write(out, base_reserved_doc(d2, t3_digest))
    print("reserved_t3_override" if st2 == "consumed" else "reserved_t3_recover")
    sys.exit(0)
PY
}

record_production_cycle_consumed() {
  local sha="$1" attempt="$2" candidate="$3" path
  path="$(production_cycle_path "$sha")"
  mkdir -p "$(dirname "$path")"
  python3 - "$path" "$sha" "$attempt" "$candidate" <<'PY' || die "cannot mark production cycle consumed for $sha"
import fcntl, json, os, sys
from datetime import datetime, timezone
out, sha, attempt, cand = sys.argv[1:]
lock_path = out + ".lock"
os.makedirs(os.path.dirname(out), exist_ok=True)
with open(lock_path, "a+", encoding="utf-8") as lockf:
    fcntl.flock(lockf.fileno(), fcntl.LOCK_EX)
    prev = {}
    if os.path.exists(out):
        try:
            prev = json.load(open(out))
        except Exception as e:
            raise SystemExit(f"ledger unreadable: {e}")
    if prev and prev.get("source_sha") not in (None, sha):
        raise SystemExit(f"ledger source_sha mismatch: {prev.get('source_sha')!r}")
    # Only the reserving attempt may consume (or legacy missing status).
    if prev.get("status") == "reserved" and prev.get("production_attempt_id") not in (
        None,
        attempt,
    ):
        raise SystemExit(
            f"cannot consume: reserved by {prev.get('production_attempt_id')!r}, "
            f"not {attempt!r}"
        )
    doc = {
        "schema_version": 1,
        "kind": "production-cycle",
        "source_sha": sha,
        "status": "consumed",
        "notarization_consumed": True,
        "production_attempt_id": attempt,
        "production_candidate_path": cand,
        "reserved_at": prev.get("reserved_at"),
        "t3_exception_path": prev.get("t3_exception_path"),
        "t3_packet_sha256": prev.get("t3_packet_sha256"),
        "spent_t3_digests": prev.get("spent_t3_digests") or [],
        "checkout_head": prev.get("checkout_head"),
        "scripts_dirty": prev.get("scripts_dirty"),
        "packaging_dirty": prev.get("packaging_dirty"),
        "prior_consumed_attempt_id": prev.get("prior_consumed_attempt_id"),
        "prior_reserved_attempt_id": prev.get("prior_reserved_attempt_id"),
        "consumed_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    tmp = out + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, sort_keys=True, indent=2)
        fh.write("\n")
    os.replace(tmp, out)
PY
}

# ---------------------------------------------------------------------------
# Helpers: attempt-effect ledger (atomic complete records; skip on verified reuse)
# ---------------------------------------------------------------------------
attempt_get() {
  local receipt="$1" key="$2"
  python3 - "$receipt" "$key" <<'PY'
import json, sys
path, key = sys.argv[1], sys.argv[2]
d = json.load(open(path))
effects = d.get("effects") or {}
print(json.dumps(effects.get(key)))
PY
}

attempt_set_effect() {
  # attempt_set_effect RECEIPT KEY JSON_OBJECT
  local receipt="$1" key="$2" payload="$3"
  python3 - "$receipt" "$key" "$payload" <<'PY'
import json, sys
from datetime import datetime, timezone
path, key, payload = sys.argv[1:]
d = json.load(open(path))
effects = d.setdefault("effects", {})
obj = json.loads(payload)
obj["updated_at"] = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
effects[key] = obj
# Keep legacy external_effects as an append-only audit trail.
trail = d.setdefault("external_effects", [])
trail.append({
  "effect": key,
  "status": obj.get("status"),
  "detail": obj.get("detail") or obj.get("candidate_path") or obj.get("images_tag") or "",
  "at": obj["updated_at"],
})
tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8") as fh:
    json.dump(d, fh, sort_keys=True, indent=2)
    fh.write("\n")
import os
os.replace(tmp, path)
PY
}

effect_status() {
  local receipt="$1" key="$2"
  python3 - "$receipt" "$key" <<'PY'
import json, sys
path, key = sys.argv[1], sys.argv[2]
d = json.load(open(path))
e = (d.get("effects") or {}).get(key) or {}
print(e.get("status") or "")
PY
}

# ---------------------------------------------------------------------------
# Local tag peel (non-hermetic publish)
# ---------------------------------------------------------------------------
# Peel a local annotated/lightweight tag to a full commit SHA, or print empty
# if the tag is absent. Do NOT use bare `git rev-parse TAG^{commit} || true`:
# on failure rev-parse echoes the input (e.g. "v0.1.3^{commit}"), which a
# first-publish path would treat as a real SHA and refuse.
local_tag_peeled_or_empty() {
  local tag="$1"
  if git rev-parse -q --verify "${tag}^{commit}" >/dev/null 2>&1; then
    git rev-parse "${tag}^{commit}"
  fi
}

# ---------------------------------------------------------------------------
# Shared preflight
# ---------------------------------------------------------------------------
preflight_machine() {
  note "preflight: machine and credentials"
  [[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
  [[ "$(uname -m)" == "arm64" ]] || die "Apple silicon only"
  command -v docker >/dev/null && docker info >/dev/null 2>&1 || die "Docker required"
  [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]] || die "APPLE_SIGNING_IDENTITY is required"
  [[ -n "${APPLE_NOTARY_PROFILE:-}" ]] || die "APPLE_NOTARY_PROFILE is required"
  security find-identity -v -p codesigning | grep -F "$APPLE_SIGNING_IDENTITY" >/dev/null \
    || die "signing identity not in keychain: $APPLE_SIGNING_IDENTITY"
  xcrun notarytool history --keychain-profile "$APPLE_NOTARY_PROFILE" --output-format json >/dev/null 2>&1 \
    || die "notary profile unusable: $APPLE_NOTARY_PROFILE"
}

preflight_source_tree() {
  note "preflight: source tree"
  [[ -z "$(git status --porcelain 2>/dev/null || true)" ]] || die "working tree is dirty"
  [[ -z "${IRIN_SMOKE_APP:-}" ]] || die "IRIN_SMOKE_APP substitution is forbidden in the release transaction"
  [[ -z "${IRIN_APP_SUPPORT_ROOT:-}" ]] || die "IRIN_APP_SUPPORT_ROOT isolation is forbidden in the release transaction"
  [[ "$HOME" == "/Users/"* && ! -L "$HOME" ]] || die "remapped HOME is forbidden in the release transaction"
}

preflight_runtime_bounds() {
  # Bounded preflight before GHCR/Apple: no IRIN process, free :8765.
  note "preflight: no IRIN process; free :8765"
  local pids
  pids="$(pgrep -f 'council-warroom-tauri|IRIN\.app/Contents/MacOS' 2>/dev/null || true)"
  if [[ -n "$pids" ]]; then
    die "IRIN process still running (pids: $(echo "$pids" | tr '\n' ' ')); stop it before preparation"
  fi
  if command -v lsof >/dev/null; then
    if lsof -nP -iTCP:8765 -sTCP:LISTEN >/dev/null 2>&1; then
      die "port :8765 is in use; free it before preparation (promotion smoke needs it)"
    fi
  else
    # Fallback: bash /dev/tcp probe
    if (echo >/dev/tcp/127.0.0.1/8765) >/dev/null 2>&1; then
      die "port :8765 appears open; free it before preparation"
    fi
  fi
}

# ---------------------------------------------------------------------------
# T1 packet validation
# ---------------------------------------------------------------------------
validate_t1_packet() {
  local path="$1"
  [[ -n "$path" ]] || die "--t1-packet PATH is required for preparation"
  [[ "$path" == /* ]] || path="$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
  [[ -f "$path" ]] || die "T1 packet missing: $path"
  python3 - "$path" <<'PY'
import json, re, sys
from datetime import datetime, timezone
path = sys.argv[1]
d = json.load(open(path))
if d.get("schema_version") != 1:
    raise SystemExit("T1 packet schema_version must be 1")
if d.get("packet_kind") != "t1":
    raise SystemExit("T1 packet_kind must be 't1'")
cid = d.get("signed_rc_candidate_id")
if not isinstance(cid, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", cid):
    raise SystemExit("T1 signed_rc_candidate_id must be 64-char hex candidate id")
sha = d.get("source_sha")
if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
    raise SystemExit("T1 source_sha must be 40-char full git SHA (hex)")
attempt = d.get("production_attempt_id")
if not attempt:
    raise SystemExit("T1 production_attempt_id missing")
effects = d.get("authorized_effects")
if not isinstance(effects, list) or not effects:
    raise SystemExit("T1 authorized_effects must be a non-empty list")
required = {"ghcr-rc-push", "apple-rc-notarization", "one-production-cycle"}
missing = sorted(required - set(effects))
if missing:
    raise SystemExit(f"T1 authorized_effects missing: {', '.join(missing)}")
expiry = d.get("expiry")
if not expiry:
    raise SystemExit("T1 expiry missing")
raw = str(expiry).strip()
if raw.endswith("Z"):
    exp = datetime.fromisoformat(raw.replace("Z", "+00:00"))
else:
    exp = datetime.fromisoformat(raw)
if exp.tzinfo is None:
    exp = exp.replace(tzinfo=timezone.utc)
if exp < datetime.now(timezone.utc):
    raise SystemExit(f"T1 authorization expired at {expiry}")
print(f'T1_SIGNED_RC_ID={json.dumps(cid)}')
print(f'T1_SOURCE_SHA={json.dumps(sha)}')
print(f'T1_ATTEMPT_ID={json.dumps(str(attempt))}')
print(f'T1_EXPIRY={json.dumps(str(expiry))}')
print(f'T1_EFFECTS_JSON={json.dumps(json.dumps(effects))}')
print(f'T1_PACKET_PATH={json.dumps(path)}')
PY
}

# ---------------------------------------------------------------------------
# Image helpers
# ---------------------------------------------------------------------------
resolve_image_digest() {
  local ref="$1"
  docker buildx imagetools inspect "$ref" --format '{{.Manifest.Digest}}' 2>/dev/null \
    || die "cannot resolve image ref: $ref"
}

image_revision() {
  local digest_ref="$1"
  docker buildx imagetools inspect "$digest_ref" \
    --format '{{index .Manifest.Annotations "org.opencontainers.image.revision"}}' 2>/dev/null \
    | tr -d '[:space:]' || true
}

resolve_rc_pair() {
  # Sets RC_GW_DIGEST RC_SC_DIGEST (sha256:…) when both rc tags resolve; else empty.
  local tag="$1" want_sha="$2"
  local gw sc rev_g rev_s
  RC_GW_DIGEST=""
  RC_SC_DIGEST=""
  gw="$(docker buildx imagetools inspect "ghcr.io/irinityhq/irin-gateway:$tag" --format '{{.Manifest.Digest}}' 2>/dev/null || true)"
  sc="$(docker buildx imagetools inspect "ghcr.io/irinityhq/irin-sidecar:$tag" --format '{{.Manifest.Digest}}' 2>/dev/null || true)"
  [[ -n "$gw" && -n "$sc" ]] || return 1
  rev_g="$(image_revision "ghcr.io/irinityhq/irin-gateway@$gw")"
  rev_s="$(image_revision "ghcr.io/irinityhq/irin-sidecar@$sc")"
  [[ "$rev_g" == "$want_sha" && "$rev_s" == "$want_sha" ]] || return 1
  RC_GW_DIGEST="$gw"
  RC_SC_DIGEST="$sc"
  return 0
}

# promote_version_labels CREATE=0|1 — CREATE=0 is validate-only (public retry).
promote_version_labels() {
  local gw_digest_ref="$1" sc_digest_ref="$2" version_tag="$3" allow_create="${4:-1}"
  local gw_image="ghcr.io/irinityhq/irin-gateway"
  local sc_image="ghcr.io/irinityhq/irin-sidecar"
  local existing want_gw want_sc
  want_gw="${gw_digest_ref#*@}"
  want_sc="${sc_digest_ref#*@}"
  case "$want_gw" in sha256:*) ;; *) want_gw="sha256:$want_gw" ;; esac
  case "$want_sc" in sha256:*) ;; *) want_sc="sha256:$want_sc" ;; esac

  if existing="$(docker buildx imagetools inspect "$gw_image:$version_tag" --format '{{.Manifest.Digest}}' 2>/dev/null)"; then
    [[ "$existing" == "$want_gw" ]] \
      || die "gateway $version_tag already resolves to $existing (candidate wants $want_gw); refusing"
    note "gateway $version_tag already matches candidate digest"
  else
    [[ "$allow_create" == "1" ]] \
      || die "gateway $version_tag missing; public-release retry must not create labels"
    note "promote gateway digest → $version_tag"
    docker buildx imagetools create --tag "$gw_image:$version_tag" "$gw_digest_ref" \
      || die "failed to label gateway $version_tag"
  fi

  if existing="$(docker buildx imagetools inspect "$sc_image:$version_tag" --format '{{.Manifest.Digest}}' 2>/dev/null)"; then
    [[ "$existing" == "$want_sc" ]] \
      || die "sidecar $version_tag already resolves to $existing (candidate wants $want_sc); refusing"
    note "sidecar $version_tag already matches candidate digest"
  else
    [[ "$allow_create" == "1" ]] \
      || die "sidecar $version_tag missing; public-release retry must not create labels"
    note "promote sidecar digest → $version_tag"
    docker buildx imagetools create --tag "$sc_image:$version_tag" "$sc_digest_ref" \
      || die "failed to label sidecar $version_tag"
  fi

  local got_gw got_sc
  got_gw="$(resolve_image_digest "$gw_image:$version_tag")"
  got_sc="$(resolve_image_digest "$sc_image:$version_tag")"
  [[ "$got_gw" == "$want_gw" ]] || die "post-check gateway label mismatch: $got_gw != $want_gw"
  [[ "$got_sc" == "$want_sc" ]] || die "post-check sidecar label mismatch: $got_sc != $want_sc"
}

# Remote annotated-tag peel: compare peeled commit, not tag-object SHA.
# Request BOTH refs/tags/$tag and refs/tags/$tag^{} — an exact pattern for only
# the unpeeled ref returns solely the annotated tag-object SHA on real remotes.
#
# Exit status:
#   0 + printed SHA  — tag present (peeled commit or lightweight)
#   0 + empty stdout  — successful lookup, tag absent
#   non-zero (die)    — lookup failure (network/auth/remote); never "absent"
remote_tag_peeled_commit() {
  local tag="$1" remote="${2:-origin}"
  local out peeled plain rc
  # Do NOT swallow git failures with || true — empty success means absent;
  # non-zero means refuse before any label mutation.
  set +e
  out="$(git ls-remote --tags "$remote" \
    "refs/tags/${tag}" "refs/tags/${tag}^{}" 2>&1)"
  rc=$?
  set -e
  if [[ $rc -ne 0 ]]; then
    die "git ls-remote failed for remote=$remote tag=$tag (exit $rc): $out"
  fi
  peeled="$(printf '%s\n' "$out" \
    | awk -v t="refs/tags/${tag}^{}" '$2 == t { print $1; exit }')"
  if [[ -n "$peeled" ]]; then
    printf '%s' "$peeled"
    return 0
  fi
  # Lightweight tag: only the unpeeled line exists and is already a commit.
  plain="$(printf '%s\n' "$out" \
    | awk -v t="refs/tags/${tag}" '$2 == t { print $1; exit }')"
  printf '%s' "$plain"
  return 0
}

# Select a tag-bound release.yml run from gh run-list JSON (env IRIN_GH_RUNS_JSON).
# Prints shell assignments: MATCHED, RUN_ID, RUN_STATUS, RUN_CONCLUSION, RUN_BRANCH.
# JSON is passed via env so a heredoc program does not steal stdin (SC2259).
select_tag_release_run() {
  local tag="$1" want_sha="$2"
  [[ -n "${IRIN_GH_RUNS_JSON+x}" ]] || die "select_tag_release_run requires IRIN_GH_RUNS_JSON"
  IRIN_GH_RUNS_JSON="$IRIN_GH_RUNS_JSON" python3 - "$tag" "$want_sha" <<'PY'
import json, os, sys

tag, want = sys.argv[1], sys.argv[2]
raw = os.environ.get("IRIN_GH_RUNS_JSON", "")
try:
    runs = json.loads(raw) if raw else []
except Exception:
    print("MATCHED=0")
    raise SystemExit(0)
if not isinstance(runs, list):
    print("MATCHED=0")
    raise SystemExit(0)


def tag_bound(r: dict) -> bool:
    """Run must be for this tag push (or SHA-only when branch empty)."""
    branch = r.get("headBranch") or ""
    title = (r.get("displayTitle") or "") + " " + (r.get("name") or "")
    if branch in (tag, f"refs/tags/{tag}"):
        return True
    if branch == "" and tag in title:
        return True
    # Tag push often sets headBranch to the bare tag name; empty branch + exact
    # SHA match from release.yml is accepted only with event=push (tag).
    if branch == "" and (r.get("event") or "") == "push":
        return True
    return False


candidates = [
    r for r in runs
    if (r.get("headSha") or "") == want and tag_bound(r)
]
if not candidates:
    print("MATCHED=0")
    raise SystemExit(0)
# In-progress first so the waiter keeps polling; else first completed match.
best = None
for r in candidates:
    if r.get("status") != "completed":
        best = r
        break
if best is None:
    best = candidates[0]
print("MATCHED=1")
print(f"RUN_ID={json.dumps(str(best.get('databaseId') or ''))}")
print(f"RUN_STATUS={json.dumps(best.get('status') or '')}")
print(f"RUN_CONCLUSION={json.dumps(best.get('conclusion') or '')}")
print(f"RUN_BRANCH={json.dumps(best.get('headBranch') or '')}")
PY
}

# Wait for the tag-bound release.yml ("IRIN Release") run to conclude success
# for the candidate source SHA. Draft existence alone is not sufficient — a
# stale/preexisting draft must not satisfy the gate.
wait_for_tag_release_workflow() {
  local tag="$1" want_sha="$2"
  local max_attempts="${IRIN_RELEASE_WORKFLOW_WAIT_ATTEMPTS:-90}"
  local sleep_s="${IRIN_RELEASE_WORKFLOW_WAIT_SLEEP:-10}"
  local i runs gh_rc
  note "wait for release.yml (IRIN Release) success bound to tag=$tag sha=$want_sha"

  for i in $(seq 1 "$max_attempts"); do
    # workflow file path is release.yml; name is "IRIN Release".
    set +e
    runs="$(gh run list --workflow=release.yml --limit 40 \
      --json databaseId,headSha,status,conclusion,headBranch,event,displayTitle,name,workflowName 2>&1)"
    gh_rc=$?
    set -e
    if [[ $gh_rc -ne 0 ]]; then
      note "gh run list failed (exit $gh_rc) attempt $i/$max_attempts: ${runs:0:200}"
      sleep "$sleep_s"
      continue
    fi
    if [[ -n "$runs" && "$runs" != "[]" ]]; then
      MATCHED=0
      RUN_ID=""
      RUN_STATUS=""
      RUN_CONCLUSION=""
      RUN_BRANCH=""
      # Pass JSON via env — never pipe into a python heredoc (SC2259).
      eval "$(IRIN_GH_RUNS_JSON="$runs" select_tag_release_run "$tag" "$want_sha")"
      if [[ "${MATCHED:-0}" == "1" ]]; then
        if [[ "$RUN_STATUS" == "completed" ]]; then
          if [[ "$RUN_CONCLUSION" == "success" ]]; then
            note "release.yml run $RUN_ID succeeded for $want_sha (branch=${RUN_BRANCH:-none})"
            return 0
          fi
          die "release.yml run $RUN_ID concluded $RUN_CONCLUSION for $want_sha; refusing to attach/publish"
        fi
        note "release.yml run $RUN_ID status=$RUN_STATUS (attempt $i/$max_attempts)"
      else
        note "no release.yml run yet for tag=$tag sha=$want_sha (attempt $i/$max_attempts)"
      fi
    else
      note "release.yml run list empty (attempt $i/$max_attempts)"
    fi
    sleep "$sleep_s"
  done
  die "timed out waiting for release.yml success on $tag @ $want_sha"
}

# gh JSON helpers — never pass --arg to gh (unsupported); pipe to jq.
gh_release_asset_browser_url() {
  local tag="$1" name="$2"
  gh api "repos/irinityhq/irin/releases/tags/${tag}" \
    | jq -r --arg n "$name" '.assets[]? | select(.name==$n) | .browser_download_url' \
    | head -1
}

gh_release_asset_id() {
  local tag="$1" name="$2"
  gh api "repos/irinityhq/irin/releases/tags/${tag}" \
    | jq -r --arg n "$name" '.assets[]? | select(.name==$n) | .id' \
    | head -1
}

gh_release_has_asset() {
  local tag="$1" name="$2"
  local id
  id="$(gh release view "$tag" --json assets \
    | jq -r --arg n "$name" '.assets[]? | select(.name==$n) | .name' | head -1)"
  [[ -n "$id" ]]
}

# ---------------------------------------------------------------------------
# --prepare-production
# ---------------------------------------------------------------------------
do_prepare() {
  eval "$(validate_t1_packet "$T1_PACKET")"
  preflight_machine
  preflight_source_tree
  preflight_runtime_bounds

  SHA="$(git rev-parse HEAD)"
  [[ "$SHA" == "$T1_SOURCE_SHA" ]] \
    || die "HEAD ($SHA) does not match T1 source_sha ($T1_SOURCE_SHA)"
  snapshot_checkout_control
  [[ "$CHECKOUT_HEAD" == "$SHA" ]] \
    || die "checkout snapshot HEAD mismatch ($CHECKOUT_HEAD vs $SHA)"

  note "resolve signed-rc candidate from T1"
  SIGNED_RC_PATH="$(
    find "$IRIN_CANDIDATE_ROOT" -type d -name "$T1_SIGNED_RC_ID" 2>/dev/null | head -1 || true
  )"
  [[ -n "$SIGNED_RC_PATH" && -d "$SIGNED_RC_PATH" ]] \
    || die "signed-rc candidate id not found under IRIN_CANDIDATE_ROOT: $T1_SIGNED_RC_ID"
  SIGNED_RC_PATH="$(cd "$SIGNED_RC_PATH" && pwd)"
  [[ "$(basename "$SIGNED_RC_PATH")" == "$T1_SIGNED_RC_ID" ]] \
    || die "signed-rc path basename does not match T1 candidate id"
  python3 - "$SIGNED_RC_PATH/candidate.json" "$T1_SOURCE_SHA" "$T1_SIGNED_RC_ID" <<'PY' \
    || die "signed-rc candidate identity does not match T1 packet"
import json, sys, hashlib
path, want_sha, want_id = sys.argv[1:]
raw = open(path, "rb").read()
d = json.loads(raw.decode("utf-8"))
if d.get("pack_mode") != "signed-rc":
    raise SystemExit(f"T1 candidate pack_mode must be signed-rc (got {d.get('pack_mode')!r})")
if d.get("source_sha") != want_sha:
    raise SystemExit("signed-rc source_sha does not match T1")
if d.get("stapled") is not False:
    raise SystemExit("signed-rc candidate must have stapled=false")
cid = hashlib.sha256(raw).hexdigest()
if cid != want_id:
    raise SystemExit(f"signed-rc candidate-id mismatch (path={want_id} recomputed={cid})")
print("ok")
PY

  note "require signed-rc Candidate verified (merged source + green CI required)"
  if ! bash "$ROOT/scripts/candidate-status.sh" \
      --candidate "$SIGNED_RC_PATH" --require "Candidate verified"; then
    bash "$ROOT/scripts/candidate-status.sh" --candidate "$SIGNED_RC_PATH" --json || true
    die "preparation refuses signed-rc below Candidate verified (source must be on main with green CI)"
  fi

  # Attempt receipt BEFORE first external effect.
  ATTEMPTS_ROOT="$IRIN_CANDIDATE_ROOT/.attempts"
  mkdir -p "$ATTEMPTS_ROOT"
  ATTEMPT_RECEIPT="$ATTEMPTS_ROOT/prepare-${T1_ATTEMPT_ID}.json"
  PACKET_HASH="$(irin_sha256_file "$T1_PACKET_PATH")"
  if [[ -f "$ATTEMPT_RECEIPT" ]]; then
    note "resuming prior prepare attempt under same T1: $T1_ATTEMPT_ID"
    python3 - "$ATTEMPT_RECEIPT" "$T1_SOURCE_SHA" "$T1_SIGNED_RC_ID" "$PACKET_HASH" \
      "$SCRIPTS_DIRTY" "$PACKAGING_DIRTY" <<'PY' \
      || die "prior attempt receipt conflicts with current T1 inputs; authorize a new attempt"
import json, os, sys
path, sha, cid, phash, scripts_dirty, packaging_dirty = sys.argv[1:]
prev = json.load(open(path))
if prev.get("source_sha") != sha:
    raise SystemExit("prior attempt source_sha differs")
if prev.get("signed_rc_candidate_id") != cid:
    raise SystemExit("prior attempt signed_rc_candidate_id differs")
if prev.get("t1_packet_sha256") and prev["t1_packet_sha256"] != phash:
    raise SystemExit("prior attempt T1 packet bytes differ; new authorization required")
if prev.get("result") == "PASS" and prev.get("production_candidate_path"):
    # Fully complete attempt — still allow re-entry to re-verify and print path.
    pass
# Require T1-time binding fields; do not backfill from present state (#0058).
if prev.get("checkout_head") is None:
    raise SystemExit(
        "prior attempt receipt lacks checkout_head; refuse unknowable history"
    )
if prev.get("checkout_head") != sha:
    raise SystemExit(
        f"prior attempt checkout_head {prev.get('checkout_head')!r} != HEAD {sha!r}"
    )
if "scripts_dirty" not in prev or "packaging_dirty" not in prev:
    raise SystemExit(
        "prior attempt receipt lacks scripts_dirty/packaging_dirty; "
        "refuse unknowable history"
    )
print("resume_ok")
PY
  else
    python3 - "$ATTEMPT_RECEIPT" "$T1_ATTEMPT_ID" "$SHA" "$T1_SIGNED_RC_ID" \
      "$PACKET_HASH" "$T1_EXPIRY" "$SCRIPTS_DIRTY" "$PACKAGING_DIRTY" <<'PY'
import json, sys
from datetime import datetime, timezone
out, attempt, sha, cid, phash, expiry, scripts_dirty, packaging_dirty = sys.argv[1:]
doc = {
  "schema_version": 1,
  "kind": "prepare-production-attempt",
  "production_attempt_id": attempt,
  "source_sha": sha,
  "checkout_head": sha,
  "scripts_dirty": scripts_dirty == "true",
  "packaging_dirty": packaging_dirty == "true",
  "signed_rc_candidate_id": cid,
  "t1_packet_sha256": phash,
  "expiry": expiry,
  "started_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "effects": {},
  "external_effects": [],
}
tmp = out + ".tmp"
with open(tmp, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
import os
os.replace(tmp, out)
PY
    note "wrote attempt receipt before external effects: $ATTEMPT_RECEIPT"
    note "checkout binding: head=$SHA scripts_dirty=$SCRIPTS_DIRTY packaging_dirty=$PACKAGING_DIRTY"
  fi

  IMAGES_TAG="rc-$(printf '%s' "$SHA" | cut -c1-12)"
  IRIN_RELEASE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' \
    "$ROOT/council-rs/warroom-tauri/src-tauri/tauri.conf.json")" \
    || die "could not read version from tauri.conf.json"
  export IRIN_RELEASE_VERSION
  echo "source_sha=$SHA images_tag=$IMAGES_TAG mode=prepare release_version=$IRIN_RELEASE_VERSION"
  echo "signed_rc=$SIGNED_RC_PATH"
  echo "t1_attempt=$T1_ATTEMPT_ID"

  # ---- production-cycle claim before first irreversible effect (#0056) ----
  # Peek production-build status: complete/recoverable skip re-reserve; fresh
  # needs exclusive reservation (T3 if already consumed) before GHCR/notary.
  PROD_ST_EARLY="$(effect_status "$ATTEMPT_RECEIPT" "production-build")"
  PROD_PATH_EARLY=""
  if [[ "$PROD_ST_EARLY" == "complete" || "$PROD_ST_EARLY" == "starting" ]]; then
    PROD_PATH_EARLY="$(python3 -c 'import json,sys; e=(json.load(open(sys.argv[1])).get("effects") or {}).get("production-build") or {}; print(e.get("candidate_path") or "")' "$ATTEMPT_RECEIPT")"
  fi
  NEED_FRESH_PROD=1
  if [[ "$PROD_ST_EARLY" == "complete" && -n "$PROD_PATH_EARLY" && -d "$PROD_PATH_EARLY" ]]; then
    NEED_FRESH_PROD=0
  elif [[ "$PROD_ST_EARLY" == "starting" && -n "$PROD_PATH_EARLY" && -d "$PROD_PATH_EARLY" ]]; then
    NEED_FRESH_PROD=0
  elif [[ "$PROD_ST_EARLY" == "starting" && -z "$PROD_PATH_EARLY" ]]; then
    # Interrupted mid-notary without path: refuse before GHCR re-entry.
    die "production-build was interrupted without a candidate path (Apple cycle may have been spent); authorize a new T1 attempt — refusing silent re-notary"
  fi
  if [[ "$NEED_FRESH_PROD" == "1" ]]; then
    CYCLE_STATE="$(production_cycle_state "$SHA")"
    T3_PATH_ABS=""
    if [[ "$CYCLE_STATE" == "consumed" || "$CYCLE_STATE" == "reserved" ]]; then
      # reserved may be same-attempt (reserve handles) or foreign abandoned
      # (requires T3). consumed always requires a fresh single-use T3.
      if [[ -n "$T3_EXCEPTION" ]]; then
        T3_PATH_ABS="$T3_EXCEPTION"
        [[ "$T3_PATH_ABS" == /* ]] || T3_PATH_ABS="$(cd "$(dirname "$T3_PATH_ABS")" && pwd)/$(basename "$T3_PATH_ABS")"
        # Shape check early for clear errors; spend/CAS happens in reserve.
        validate_t3_exception "$T3_PATH_ABS" "$SHA" >/dev/null \
          || die "T3 exception invalid for source $SHA"
        note "T3 exception provided for cycle claim on $SHA (state=$CYCLE_STATE)"
      fi
    elif [[ -n "$T3_EXCEPTION" ]]; then
      T3_PATH_ABS="$T3_EXCEPTION"
      [[ "$T3_PATH_ABS" == /* ]] || T3_PATH_ABS="$(cd "$(dirname "$T3_PATH_ABS")" && pwd)/$(basename "$T3_PATH_ABS")"
      validate_t3_exception "$T3_PATH_ABS" "$SHA" >/dev/null || die "T3 exception invalid"
    fi
    # Bind T1-time dirty flags from the attempt receipt (authoritative).
    eval "$(python3 - "$ATTEMPT_RECEIPT" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print(f'BIND_HEAD={json.dumps(d.get("checkout_head") or "")}')
print(f'BIND_SCRIPTS={json.dumps("true" if d.get("scripts_dirty") else "false")}')
print(f'BIND_PACKAGING={json.dumps("true" if d.get("packaging_dirty") else "false")}')
PY
)"
    [[ "$BIND_HEAD" == "$SHA" ]] \
      || die "attempt receipt checkout_head missing/mismatched ($BIND_HEAD vs $SHA)"
    reserve_production_cycle "$SHA" "$T1_ATTEMPT_ID" "$T3_PATH_ABS" \
      "$BIND_HEAD" "$BIND_SCRIPTS" "$BIND_PACKAGING" \
      || die "failed to reserve production cycle for $SHA"
    note "production-cycle reserved for attempt $T1_ATTEMPT_ID (before external effects)"
  fi

  # ---- effect: ghcr-rc-push (once) ----------------------------------------
  GHCR_ST="$(effect_status "$ATTEMPT_RECEIPT" "ghcr-rc-push")"
  SKIP_GHCR=0
  if [[ "$GHCR_ST" == "complete" ]]; then
    eval "$(python3 - "$ATTEMPT_RECEIPT" <<'PY'
import json, sys
e = (json.load(open(sys.argv[1])).get("effects") or {}).get("ghcr-rc-push") or {}
print(f'REC_GW={json.dumps(e.get("gateway_digest") or "")}')
print(f'REC_SC={json.dumps(e.get("sidecar_digest") or "")}')
print(f'REC_TAG={json.dumps(e.get("images_tag") or "")}')
PY
)"
    if [[ "$REC_TAG" == "$IMAGES_TAG" ]] && resolve_rc_pair "$IMAGES_TAG" "$SHA"; then
      if [[ "$RC_GW_DIGEST" == "$REC_GW" && "$RC_SC_DIGEST" == "$REC_SC" ]]; then
        note "ghcr-rc-push already complete; reusing digests $RC_GW_DIGEST / $RC_SC_DIGEST"
        SKIP_GHCR=1
      else
        die "recorded rc digests no longer match live registry; authorize a new attempt"
      fi
    else
      die "cannot re-verify completed ghcr-rc-push; authorize a new attempt"
    fi
  elif [[ "$GHCR_ST" == "starting" ]]; then
    # Crash mid-push: recover ONLY if both images are conclusively present with
    # the correct source-SHA revision. Partial push / transient registry read
    # must NOT re-invoke the push under the same T1 (one-production-cycle).
    if resolve_rc_pair "$IMAGES_TAG" "$SHA"; then
      note "recovering ghcr-rc-push from live registry digests"
      attempt_set_effect "$ATTEMPT_RECEIPT" "ghcr-rc-push" "$(python3 -c 'import json; print(json.dumps({
        "status": "complete",
        "images_tag": "'"$IMAGES_TAG"'",
        "gateway_digest": "'"$RC_GW_DIGEST"'",
        "sidecar_digest": "'"$RC_SC_DIGEST"'",
      }))')"
      SKIP_GHCR=1
    else
      die "ghcr-rc-push was interrupted without both SHA-bound digests recoverable; authorize a new T1 attempt — refusing silent re-push"
    fi
  fi

  if [[ "$SKIP_GHCR" != "1" ]]; then
    # Only a never-started effect may enter the push path under this T1.
    [[ -z "$GHCR_ST" || "$GHCR_ST" == "null" ]] \
      || die "ghcr-rc-push status=$GHCR_ST is not eligible for a fresh push; authorize a new attempt"
    attempt_set_effect "$ATTEMPT_RECEIPT" "ghcr-rc-push" \
      '{"status":"starting","images_tag":"'"$IMAGES_TAG"'"}'
    note "images: push rc-* GHCR digests (T1-authorized, one cycle)"
    command -v rsync >/dev/null || die "rsync required for the image context"
    IRIN_PACK_IMAGES_TAG="$IMAGES_TAG" bash scripts/build-gateway-pack-prod-images.sh
    resolve_rc_pair "$IMAGES_TAG" "$SHA" \
      || die "rc images missing or revision mismatch after push"
    attempt_set_effect "$ATTEMPT_RECEIPT" "ghcr-rc-push" "$(python3 -c 'import json; print(json.dumps({
      "status": "complete",
      "images_tag": "'"$IMAGES_TAG"'",
      "gateway_digest": "'"$RC_GW_DIGEST"'",
      "sidecar_digest": "'"$RC_SC_DIGEST"'",
    }))')"
  fi

  note "manifest: generate from the live registry"
  IRIN_PACK_IMAGES_TAG="$IMAGES_TAG" \
  IRIN_PACK_IMAGES_SOURCE_SHA="$SHA" \
    bash scripts/generate-production-manifest.sh
  MANIFEST="$ROOT/packaging/build/gateway-pack/image-manifest.production.json"
  grep -q '"mode"[[:space:]]*:[[:space:]]*"production"' "$MANIFEST" || die "manifest is not production"
  grep -E '"gateway"|"sidecar"' "$MANIFEST" | grep -q 'ghcr.io/irinityhq/.*@sha256:' \
    || die "manifest images are not pinned ghcr digests"
  grep -E '"gateway"|"sidecar"' "$MANIFEST" | grep -qE 'example|irin-desktop/' \
    && die "manifest contains placeholder/local refs"

  # ---- effect: production-build / apple notary (once) ---------------------
  # Cycle already reserved before GHCR when NEED_FRESH_PROD=1. Complete path
  # updates reserved → consumed after candidate_path is known.
  PROD_ST="$(effect_status "$ATTEMPT_RECEIPT" "production-build")"
  CANDIDATE_PATH=""
  SKIP_PROD=0
  if [[ "$PROD_ST" == "complete" ]]; then
    CANDIDATE_PATH="$(python3 -c 'import json,sys; e=(json.load(open(sys.argv[1])).get("effects") or {}).get("production-build") or {}; print(e.get("candidate_path") or "")' "$ATTEMPT_RECEIPT")"
    if [[ -n "$CANDIDATE_PATH" && -d "$CANDIDATE_PATH" && -f "$CANDIDATE_PATH/candidate.json" ]]; then
      python3 - "$CANDIDATE_PATH/candidate.json" "$SHA" <<'PY' || die "recorded production candidate identity mismatch"
import json, sys
d = json.load(open(sys.argv[1]))
if d.get("pack_mode") != "production" or d.get("source_sha") != sys.argv[2] or not d.get("stapled"):
    raise SystemExit("bad production candidate identity")
print("ok")
PY
      note "production-build already complete; reusing $CANDIDATE_PATH"
      SKIP_PROD=1
      # Bind/upgrade cycle ledger if an older attempt predated status field.
      if [[ "$(production_cycle_state "$SHA")" != "consumed" ]]; then
        record_production_cycle_consumed "$SHA" "$T1_ATTEMPT_ID" "$CANDIDATE_PATH"
      fi
    else
      die "completed production-build path missing; authorize a new attempt"
    fi
  elif [[ "$PROD_ST" == "starting" ]]; then
    # Crash during notary/build: only reuse if path already recorded mid-flight.
    CANDIDATE_PATH="$(python3 -c 'import json,sys; e=(json.load(open(sys.argv[1])).get("effects") or {}).get("production-build") or {}; print(e.get("candidate_path") or "")' "$ATTEMPT_RECEIPT")"
    if [[ -n "$CANDIDATE_PATH" && -d "$CANDIDATE_PATH" ]]; then
      note "recovering interrupted production-build at $CANDIDATE_PATH"
      attempt_set_effect "$ATTEMPT_RECEIPT" "production-build" "$(python3 -c 'import json; print(json.dumps({
        "status": "complete",
        "candidate_path": "'"$CANDIDATE_PATH"'",
      }))')"
      SKIP_PROD=1
      if [[ "$(production_cycle_state "$SHA")" != "consumed" ]]; then
        record_production_cycle_consumed "$SHA" "$T1_ATTEMPT_ID" "$CANDIDATE_PATH"
      fi
    else
      die "production-build was interrupted without a candidate path (Apple cycle may have been spent); authorize a new T1 attempt — refusing silent re-notary"
    fi
  fi

  if [[ "$SKIP_PROD" != "1" ]]; then
    # Reservation was claimed before GHCR; mark attempt effect starting, then
    # build. Consumption is recorded as soon as candidate_path is known.
    attempt_set_effect "$ATTEMPT_RECEIPT" "production-build" \
      '{"status":"starting"}'
    note "dmg: production build (sign + notarize + staple) into candidate store — one cycle"
    IRIN_DMG_PACK_MODE=production \
    IRIN_GATEWAY_PACK_PROD_MANIFEST="$MANIFEST" \
    IRIN_DMG_REQUIRE_CLEAN=1 \
    IRIN_RELEASE_VERSION="$IRIN_RELEASE_VERSION" \
    IRIN_TAURI_BUILD_GIT_SHA="$SHA" \
      make dmg-build | tee /tmp/irin-prepare-dmg-$$.log
    CANDIDATE_PATH="$(awk -F= '/^candidate_path=/{print $2}' /tmp/irin-prepare-dmg-$$.log | tail -1)"
    rm -f /tmp/irin-prepare-dmg-$$.log
    [[ -n "$CANDIDATE_PATH" && -d "$CANDIDATE_PATH" ]] \
      || die "production build did not emit candidate_path="
    # Record path immediately so a crash after build still recovers without re-notary.
    attempt_set_effect "$ATTEMPT_RECEIPT" "production-build" "$(python3 -c 'import json; print(json.dumps({
      "status": "complete",
      "candidate_path": "'"$CANDIDATE_PATH"'",
    }))')"
    record_production_cycle_consumed "$SHA" "$T1_ATTEMPT_ID" "$CANDIDATE_PATH"
    note "recorded production-cycle consumption for $SHA"
  fi

  note "verify + smoke the production candidate"
  IRIN_CANDIDATE_PATH="$CANDIDATE_PATH" \
  IRIN_DMG_PACK_MODE=production \
    make dmg-verify
  IRIN_CANDIDATE_PATH="$CANDIDATE_PATH" \
  PROMOTION=1 \
    bash packaging/smoke-full-app.sh

  python3 - "$ATTEMPT_RECEIPT" "$CANDIDATE_PATH" <<'PY'
import json, sys, os
from datetime import datetime, timezone
path, cand = sys.argv[1:]
d = json.load(open(path))
d["production_candidate_path"] = cand
d["completed_at"] = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
d["result"] = "PASS"
tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8") as fh:
    json.dump(d, fh, sort_keys=True, indent=2)
    fh.write("\n")
os.replace(tmp, path)
PY

  note "prepare-production complete (no tag, no GitHub Release mutation)"
  echo "candidate_path=$CANDIDATE_PATH"
  echo "NEXT: install-verify-candidate → record-acceptance (T2) → release-transaction --publish"
}

# ---------------------------------------------------------------------------
# --publish
# ---------------------------------------------------------------------------
do_publish() {
  [[ -n "$TAG" ]] || die "--publish requires --tag vX.Y.Z"
  [[ -n "$CANDIDATE_ARG" ]] || die "--publish requires --candidate ABSOLUTE_STORE_PATH"
  [[ -n "$T2_PACKET" ]] || die "--publish requires --t2-packet CANDIDATE/proofs/t2.json"
  [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "bad tag: $TAG"
  command -v gh >/dev/null || die "gh CLI required for publish"
  command -v docker >/dev/null || die "docker required for version-label promotion"
  command -v jq >/dev/null || die "jq required for release asset JSON parsing"

  export IRIN_CANDIDATE_PATH="$CANDIDATE_ARG"
  irin_require_candidate_path
  CANDIDATE="$IRIN_CANDIDATE_PATH"
  CANDIDATE_ID="$(basename "$CANDIDATE")"

  T2_PATH="$T2_PACKET"
  [[ "$T2_PATH" == /* ]] || T2_PATH="$(cd "$(dirname "$T2_PATH")" && pwd)/$(basename "$T2_PATH")"
  [[ -f "$T2_PATH" ]] || die "T2 packet missing: $T2_PATH"
  EXPECTED_T2="$CANDIDATE/proofs/t2.json"
  [[ "$(cd "$(dirname "$T2_PATH")" && pwd)/$(basename "$T2_PATH")" == "$EXPECTED_T2" ]] \
    || die "--t2-packet must be $EXPECTED_T2"

  note "require Accepted tier via candidate-status"
  if ! bash "$ROOT/scripts/candidate-status.sh" --candidate "$CANDIDATE" --require "Accepted"; then
    bash "$ROOT/scripts/candidate-status.sh" --candidate "$CANDIDATE" --json || true
    die "publish refuses candidates below Accepted (see candidate-status above)"
  fi

  eval "$(python3 - "$CANDIDATE/candidate.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print(f'SOURCE_SHA={json.dumps(d["source_sha"])}')
print(f'SEMVER={json.dumps(d["semver"])}')
print(f'PACK_MODE={json.dumps(d["pack_mode"])}')
print(f'DMG_SHA256={json.dumps(d["dmg_sha256"])}')
print(f'GATEWAY_DIGEST={json.dumps(d["gateway_digest"])}')
print(f'SIDECAR_DIGEST={json.dumps(d["sidecar_digest"])}')
print(f'STAPLED={json.dumps("true" if d.get("stapled") else "false")}')
PY
)"
  [[ "$PACK_MODE" == "production" ]] || die "publish requires production pack_mode (got $PACK_MODE)"
  [[ "$STAPLED" == "true" ]] || die "publish requires stapled production candidate"
  [[ "v$SEMVER" == "$TAG" ]] || die "tag $TAG does not match candidate semver v$SEMVER"

  # First publication only: point-in-time live /Applications equality before any
  # mutation. Never installs. Skip hermetic rehearsal and already-published retry
  # so a later installed release cannot retroactively invalidate an older Published
  # candidate. Avoid naming the publication proof file token before public
  # re-download so static publish-order contracts keep binding the write site.
  local pub_proof_path="$CANDIDATE/proofs/publication"".json"
  if [[ ! -f "$pub_proof_path" ]] && ! publish_hermetic_active; then
    note "first-publish: require live app manifest equals candidate (point-in-time)"
    require_live_app_matches_candidate "$CANDIDATE"
  elif [[ -f "$pub_proof_path" ]]; then
    note "publication proof present: skip first-publish live app gate"
  else
    note "hermetic: skip first-publish live app gate"
  fi

  # Publish binds to T1-time control-plane fields recorded on the production-
  # cycle ledger (and requires both dirty flags recorded false). Hermetic
  # rehearsal skips live binding (#0056/#0058).
  if ! publish_hermetic_active; then
    snapshot_checkout_control
    [[ "$CHECKOUT_HEAD" == "$SOURCE_SHA" ]] \
      || die "publish requires checkout HEAD=$SOURCE_SHA (got $CHECKOUT_HEAD)"
    CYCLE_LEDGER="$(production_cycle_path "$SOURCE_SHA")"
    [[ -f "$CYCLE_LEDGER" ]] \
      || die "publish requires production-cycle ledger for $SOURCE_SHA (missing T1 bind)"
    python3 - "$CYCLE_LEDGER" "$SOURCE_SHA" "$CHECKOUT_HEAD" <<'PY' \
      || die "publish T1 checkout binding refused (see stderr)"
import json, sys
path, want_sha, head = sys.argv[1:]
d = json.load(open(path))
if d.get("source_sha") != want_sha:
    raise SystemExit(f"cycle ledger source_sha mismatch: {d.get('source_sha')!r}")
if "checkout_head" not in d or "scripts_dirty" not in d or "packaging_dirty" not in d:
    raise SystemExit(
        "cycle ledger lacks checkout_head/scripts_dirty/packaging_dirty; "
        "refuse legacy unknowable history"
    )
if d.get("checkout_head") != head:
    raise SystemExit(
        f"recorded checkout_head {d.get('checkout_head')!r} != HEAD {head!r}"
    )
if d.get("checkout_head") != want_sha:
    raise SystemExit(
        f"recorded checkout_head {d.get('checkout_head')!r} != source {want_sha!r}"
    )
if d.get("scripts_dirty") is not False:
    raise SystemExit(
        f"recorded scripts_dirty must be false at T1 (got {d.get('scripts_dirty')!r})"
    )
if d.get("packaging_dirty") is not False:
    raise SystemExit(
        f"recorded packaging_dirty must be false at T1 (got {d.get('packaging_dirty')!r})"
    )
# Live tree must also be clean at publish (current state).
print("publish_bind_ok")
PY
    [[ "$SCRIPTS_DIRTY" == "false" && "$PACKAGING_DIRTY" == "false" ]] \
      || die "publish requires clean scripts/ and packaging/ now (scripts_dirty=$SCRIPTS_DIRTY packaging_dirty=$PACKAGING_DIRTY)"
  else
    note "hermetic: skip live checkout HEAD/dirty bind"
  fi

  python3 - "$T2_PATH" "$CANDIDATE_ID" "$SOURCE_SHA" <<'PY' || die "T2 packet invalid for publication"
import json, sys
from datetime import datetime, timezone
path, cid, sha = sys.argv[1:]
d = json.load(open(path))
if d.get("proof_kind") != "t2" or d.get("result") != "PASS":
    raise SystemExit("t2 proof_kind/result invalid")
if d.get("candidate_id") != cid or d.get("source_sha") != sha:
    raise SystemExit("t2 identity mismatch")
effects = set(d.get("authorized_effects") or [])
for need in ("tag-push", "release-attach", "publish", "version-image-labels"):
    if need not in effects:
        raise SystemExit(f"t2 missing authorized effect: {need}")
raw = str(d.get("expiry", "")).strip()
if raw.endswith("Z"):
    exp = datetime.fromisoformat(raw.replace("Z", "+00:00"))
else:
    exp = datetime.fromisoformat(raw)
if exp.tzinfo is None:
    exp = exp.replace(tzinfo=timezone.utc)
if exp < datetime.now(timezone.utc):
    raise SystemExit(f"t2 authorization expired at {d.get('expiry')}")
print("t2_ok")
PY

  DMG="$(find "$CANDIDATE" -maxdepth 1 -type f -name '*.dmg' | head -1 || true)"
  [[ -n "$DMG" && -f "$DMG" ]] || die "candidate DMG missing"
  ACTUAL_DMG="$(irin_sha256_file "$DMG")"
  [[ "$ACTUAL_DMG" == "$DMG_SHA256" ]] || die "DMG bytes do not match candidate identity"
  ASSET_NAME="$(basename "$DMG")"

  case "$GATEWAY_DIGEST" in
    sha256:*) GW_REF="ghcr.io/irinityhq/irin-gateway@$GATEWAY_DIGEST" ;;
    *) GW_REF="ghcr.io/irinityhq/irin-gateway@sha256:$GATEWAY_DIGEST" ;;
  esac
  case "$SIDECAR_DIGEST" in
    sha256:*) SC_REF="ghcr.io/irinityhq/irin-sidecar@$SIDECAR_DIGEST" ;;
    *) SC_REF="ghcr.io/irinityhq/irin-sidecar@sha256:$SIDECAR_DIGEST" ;;
  esac

  # ---- remote tag (peeled) before any mutation ----------------------------
  note "check remote tag peeled commit vs candidate source (before any mutation)"
  local hermetic=0
  if publish_hermetic_active; then
    hermetic=1
  fi
  if [[ "$hermetic" == "1" ]]; then
    # Empty (unset or "") = tag absent. Set to a full SHA to simulate a peel.
    REMOTE_PEELED="${IRIN_PUBLISH_REMOTE_TAG_SHA-}"
    note "hermetic: remote tag peel override (empty=absent)"
  else
    REMOTE_PEELED="$(remote_tag_peeled_commit "$TAG")"
  fi
  if [[ -n "$REMOTE_PEELED" ]]; then
    [[ "$REMOTE_PEELED" == "$SOURCE_SHA" ]] \
      || die "remote tag $TAG peels to $REMOTE_PEELED, candidate wants $SOURCE_SHA; refusing"
    note "remote tag $TAG peels to candidate source $SOURCE_SHA"
  fi
  if [[ "$hermetic" == "1" ]]; then
    # Never inspect or mutate real tags under hermetic rehearsal.
    LOCAL_TAG_SHA=""
  else
    LOCAL_TAG_SHA="$(local_tag_peeled_or_empty "$TAG")"
    if [[ -n "$LOCAL_TAG_SHA" && "$LOCAL_TAG_SHA" != "$SOURCE_SHA" ]]; then
      die "local tag $TAG points at $LOCAL_TAG_SHA, candidate wants $SOURCE_SHA"
    fi
  fi

  # ---- release draft/public state before label mutation -------------------
  note "resolve release draft/public state before label or asset mutation"
  RELEASE_STATE="missing"
  IS_DRAFT=""
  if gh release view "$TAG" --json isDraft,url >/dev/null 2>&1; then
    IS_DRAFT="$(gh release view "$TAG" --json isDraft --jq '.isDraft')"
    if [[ "$IS_DRAFT" == "true" ]]; then
      RELEASE_STATE="draft"
    else
      RELEASE_STATE="public"
    fi
  fi
  echo "release_state=$RELEASE_STATE"

  note "GHCR login for version-label operations"
  if [[ -z "${GHCR_TOKEN:-}" ]] && command -v gh >/dev/null; then
    GHCR_TOKEN="$(gh auth token 2>/dev/null || true)"
    GHCR_USERNAME="${GHCR_USERNAME:-$(gh api user --jq .login 2>/dev/null || true)}"
  fi
  [[ -n "${GHCR_USERNAME:-}" && -n "${GHCR_TOKEN:-}" ]] \
    || die "publish requires gh auth (write:packages) or GHCR_USERNAME + GHCR_TOKEN"
  if [[ "$hermetic" == "1" ]]; then
    note "hermetic: skip docker login (fake docker on PATH handles imagetools)"
  else
    echo "${GHCR_TOKEN}" | docker login ghcr.io -u "${GHCR_USERNAME}" --password-stdin \
      || die "GHCR login failed"
  fi

  if [[ "$RELEASE_STATE" == "public" ]]; then
    # Validation-only: never create labels, never upload, never edit release.
    note "release already public — validation-only retry (no label create, no mutation)"
    promote_version_labels "$GW_REF" "$SC_REF" "$TAG" 0
  else
    # Draft or missing release: may create version labels.
    note "promote Gateway/sidecar digests to immutable $TAG labels"
    promote_version_labels "$GW_REF" "$SC_REF" "$TAG" 1

    if [[ "$hermetic" == "1" ]]; then
      note "hermetic: skip git tag create/push (workflow wait uses SOURCE_SHA only)"
      # Simulate remote tag present at source after the (skipped) push so a second
      # peel check would agree if it ran; workflow wait still binds SHA.
      IRIN_PUBLISH_REMOTE_TAG_SHA="$SOURCE_SHA"
    else
      note "git tag $TAG at candidate source SHA"
      if [[ -z "$LOCAL_TAG_SHA" ]]; then
        git tag -a "$TAG" "$SOURCE_SHA" -m "IRIN $TAG"
      fi
      # Re-check remote peel after local create, before push.
      REMOTE_PEELED="$(remote_tag_peeled_commit "$TAG")"
      if [[ -n "$REMOTE_PEELED" ]]; then
        [[ "$REMOTE_PEELED" == "$SOURCE_SHA" ]] \
          || die "remote tag $TAG peels to $REMOTE_PEELED after local create; refusing push"
      else
        git push origin "refs/tags/$TAG"
      fi
    fi

    # Workflow success is authoritative; draft existence alone is not enough
    # (a stale/preexisting draft must not satisfy the gate).
    wait_for_tag_release_workflow "$TAG" "$SOURCE_SHA"

    note "require draft release created for this tag after workflow success"
    local draft_ok=0
    local draft_attempts="${IRIN_RELEASE_DRAFT_WAIT_ATTEMPTS:-30}"
    local draft_sleep="${IRIN_RELEASE_DRAFT_WAIT_SLEEP:-2}"
    for _ in $(seq 1 "$draft_attempts"); do
      if gh release view "$TAG" --json isDraft,tagName >/dev/null 2>&1; then
        IS_DRAFT="$(gh release view "$TAG" --json isDraft --jq '.isDraft')"
        if [[ "$IS_DRAFT" == "true" ]]; then
          draft_ok=1
          break
        fi
        # If already public between workflow success and our check, fall to public path.
        if [[ "$IS_DRAFT" == "false" ]]; then
          die "release $TAG became non-draft before DMG attach; re-run publish (public validation path)"
        fi
      fi
      sleep "$draft_sleep"
    done
    [[ "$draft_ok" == "1" ]] \
      || die "draft release $TAG not found after release.yml success (stale-missing draft refuses)"

    note "draft release: upload DMG without --clobber"
    if gh_release_has_asset "$TAG" "$ASSET_NAME"; then
      note "asset already on draft; download and compare hash"
      TMP_DL="$(mktemp)"
      gh release download "$TAG" -p "$ASSET_NAME" -O "$TMP_DL" --clobber 2>/dev/null \
        || die "could not download existing draft asset for comparison"
      EXISTING_HASH="$(irin_sha256_file "$TMP_DL")"
      rm -f "$TMP_DL"
      if [[ "$EXISTING_HASH" == "$DMG_SHA256" ]]; then
        note "existing draft asset hash matches — idempotent skip upload"
      else
        die "draft asset $ASSET_NAME hash $EXISTING_HASH != candidate $DMG_SHA256; refusing (no clobber)"
      fi
    else
      gh release upload "$TAG" "$DMG"
    fi

    note "authenticated draft re-download (upload integrity only; not Published)"
    TMP_DRAFT="$(mktemp)"
    gh release download "$TAG" -p "$ASSET_NAME" -O "$TMP_DRAFT" --clobber
    DRAFT_HASH="$(irin_sha256_file "$TMP_DRAFT")"
    rm -f "$TMP_DRAFT"
    [[ "$DRAFT_HASH" == "$DMG_SHA256" ]] \
      || die "authenticated draft re-download hash mismatch: $DRAFT_HASH"

    note "pre-publish re-resolve version image labels"
    promote_version_labels "$GW_REF" "$SC_REF" "$TAG" 1

    note "publish release under T2"
    gh release edit "$TAG" --draft=false
  fi

  note "unauthenticated public re-download proves Published"
  PUBLIC_URL="$(gh_release_asset_browser_url "$TAG" "$ASSET_NAME")"
  [[ -n "$PUBLIC_URL" && "$PUBLIC_URL" != "null" ]] \
    || die "could not resolve public browser_download_url for $ASSET_NAME"
  TMP_PUB="$(mktemp)"
  curl -fsSL --proto '=https' --tlsv1.2 \
    -H 'Authorization:' -H 'Cookie:' \
    "$PUBLIC_URL" -o "$TMP_PUB" \
    || die "unauthenticated public download failed: $PUBLIC_URL"
  PUB_HASH="$(irin_sha256_file "$TMP_PUB")"
  rm -f "$TMP_PUB"
  [[ "$PUB_HASH" == "$DMG_SHA256" ]] \
    || die "public re-download hash $PUB_HASH != accepted candidate $DMG_SHA256"

  RELEASE_ID="$(gh api "repos/irinityhq/irin/releases/tags/$TAG" | jq -r '.id')"
  ASSET_ID="$(gh_release_asset_id "$TAG" "$ASSET_NAME")"
  RELEASE_HTML="$(gh api "repos/irinityhq/irin/releases/tags/$TAG" | jq -r '.html_url')"

  if [[ -f "$CANDIDATE/proofs/publication.json" ]]; then
    note "publication proof already present; re-validate via candidate-status"
  else
    note "write proofs/publication.json"
    PUB_EXTRA="$(
      python3 - <<PY
import json
print(json.dumps({
  "public_state": "published",
  "redownload_unauthenticated": True,
  "asset_sha256": "$DMG_SHA256",
  "dmg_sha256": "$DMG_SHA256",
  "tag": "$TAG",
  "repo": "irinityhq/irin",
  "release_url": "$RELEASE_HTML",
  "release_id": str($RELEASE_ID),
  "asset_name": "$ASSET_NAME",
  "asset_id": str($ASSET_ID),
  "public_download_url": "$PUBLIC_URL",
}))
PY
    )"
    irin_write_proof_envelope \
      "$CANDIDATE/proofs/publication.json" \
      "publication" \
      "$CANDIDATE_ID" \
      "$SOURCE_SHA" \
      "PASS" \
      "$PUB_EXTRA"
  fi

  bash "$ROOT/scripts/candidate-status.sh" --candidate "$CANDIDATE" --require "Published" \
    || die "candidate-status did not reach Published after publication proof"

  note "publish complete"
  echo "candidate_path=$CANDIDATE"
  echo "tag=$TAG"
  echo "release_url=$RELEASE_HTML"
  echo "public_asset_sha256=$PUB_HASH"
}

# Library mode (tests): helpers loaded; no dispatch.
if [[ "${IRIN_RELEASE_TX_LIB:-}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

case "$MODE" in
  prepare) do_prepare ;;
  publish) do_publish ;;
  *) die "unknown mode: $MODE" ;;
esac
