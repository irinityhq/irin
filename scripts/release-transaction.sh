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
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

MODE=""
TAG=""
CANDIDATE_ARG=""
T1_PACKET=""
T2_PACKET=""

usage() {
  cat <<'EOF'
Usage:
  release-transaction.sh --prepare-production --t1-packet PATH
  release-transaction.sh --publish --tag vX.Y.Z \
      --candidate ABSOLUTE_STORE_PATH --t2-packet CANDIDATE/proofs/t2.json

  --prepare-production is T1-authorized RC preparation with irreversible
  external effects (rc-* GHCR push, Apple notary once per attempt). It is not
  a no-effect simulation. There is no --dry-run-rc alias.

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
      -h|--help) usage; exit 0 ;;
      *) die "unknown argument: $1 (try --help)" ;;
    esac
  done
  [[ -n "$MODE" ]] || { usage >&2; die "mode required"; }
fi

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
import json, sys
from datetime import datetime, timezone
path = sys.argv[1]
d = json.load(open(path))
if d.get("schema_version") != 1:
    raise SystemExit("T1 packet schema_version must be 1")
if d.get("packet_kind") != "t1":
    raise SystemExit("T1 packet_kind must be 't1'")
cid = d.get("signed_rc_candidate_id")
if not isinstance(cid, str) or len(cid) != 64:
    raise SystemExit("T1 signed_rc_candidate_id must be 64-char hex candidate id")
sha = d.get("source_sha")
if not isinstance(sha, str) or len(sha) != 40:
    raise SystemExit("T1 source_sha must be 40-char full git SHA")
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
    python3 - "$ATTEMPT_RECEIPT" "$T1_SOURCE_SHA" "$T1_SIGNED_RC_ID" "$PACKET_HASH" <<'PY' \
      || die "prior attempt receipt conflicts with current T1 inputs; authorize a new attempt"
import json, sys
path, sha, cid, phash = sys.argv[1:]
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
print("resume_ok")
PY
  else
    python3 - "$ATTEMPT_RECEIPT" "$T1_ATTEMPT_ID" "$SHA" "$T1_SIGNED_RC_ID" \
      "$PACKET_HASH" "$T1_EXPIRY" <<'PY'
import json, sys
from datetime import datetime, timezone
out, attempt, sha, cid, phash, expiry = sys.argv[1:]
doc = {
  "schema_version": 1,
  "kind": "prepare-production-attempt",
  "production_attempt_id": attempt,
  "source_sha": sha,
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
  fi

  IMAGES_TAG="rc-$(printf '%s' "$SHA" | cut -c1-12)"
  IRIN_RELEASE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' \
    "$ROOT/council-rs/warroom-tauri/src-tauri/tauri.conf.json")" \
    || die "could not read version from tauri.conf.json"
  export IRIN_RELEASE_VERSION
  echo "source_sha=$SHA images_tag=$IMAGES_TAG mode=prepare release_version=$IRIN_RELEASE_VERSION"
  echo "signed_rc=$SIGNED_RC_PATH"
  echo "t1_attempt=$T1_ATTEMPT_ID"

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
    else
      die "production-build was interrupted without a candidate path (Apple cycle may have been spent); authorize a new T1 attempt — refusing silent re-notary"
    fi
  fi

  if [[ "$SKIP_PROD" != "1" ]]; then
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
