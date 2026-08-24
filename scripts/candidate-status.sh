#!/usr/bin/env bash
# candidate-status.sh — sole reporter for candidate-derived shipping tiers.
#
# Read-only. Recomputes the highest valid tier from candidate content + current
# bytes. Acceptance (and other operator fields) may be recorded in files; tier
# is always derived. Hand-edited files never advance state by presence alone.
#
# Interface:
#   candidate-status.sh --candidate ABSOLUTE_PATH [--json] [--require TIER]
#
# Valid --require TIER values:
#   Candidate verified | Installed | Accepted | Published
#
# Exit codes:
#   0  well-formed candidate (and --require met when given)
#   1  well-formed candidate below --require
#   2  usage / path / malformed-candidate refusal
#
# File → tier map (sole authority; board must call this script, not reimplement):
#   immutable payload + candidate.json + proofs/verify.json
#       → Candidate verified
#     (also requires: source SHA reachable from fetched origin/main, and
#      GitHub "CI required" aggregate green for that SHA; network/auth
#      unavailability leaves the candidate below Candidate verified)
#   proofs/install.json + matching digests under install/
#       → Installed
#   proofs/acceptance.json + candidate/effect-bound proofs/t2.json
#     referencing the acceptance digest
#       → Accepted (only while hashes still match)
#   proofs/publication.json (public-state + unauthenticated re-download hash)
#       → Published
#   NOTARY-* logs are supporting evidence only; not a tier by themselves.
#
# Caveat: Accepted does not cryptographically prove who typed a structurally
# valid receipt. The human boundary is the operator-controlled T2 action.
#
# Frozen JSON adapter schema (schema_version=1) fields for W3 ship-board join:
#   schema_version, reporter, candidate_path, candidate_id, source_sha, semver,
#   pack_mode, dmg_sha256, bundle_manifest_digest, well_formed, tier, blockers[],
#   checks{}, caveats[]
#   blockers[]: {code, message, blocks_tier}
#   checks: identity_ok, payload_ok, source_on_main, ci_required_green,
#           verify_proof, install_proof, acceptance_proof, publication_proof
#   tier: null | "Candidate verified" | "Installed" | "Accepted" | "Published"
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die_usage() { printf 'ERROR: %s\n' "$*" >&2; exit 2; }
die_eval()  { printf 'ERROR: %s\n' "$*" >&2; exit 2; }

usage() {
  cat <<'EOF'
Usage: candidate-status.sh --candidate ABSOLUTE_PATH [--json] [--require TIER]

Sole candidate-tier reporter. Recomputes highest valid tier from content.

  --candidate PATH   Absolute candidate directory under IRIN_CANDIDATE_ROOT
  --json             Machine-readable status (frozen adapter schema for ship-board)
  --require TIER     Exit 1 if current tier is below TIER
                     TIER: Candidate verified | Installed | Accepted | Published

Hermetic test hooks (ignored unless BOTH conditions hold):
  1) IRIN_CANDIDATE_STATUS_HERMETIC=1
  2) IRIN_CANDIDATE_ROOT is physically under a temp prefix
     (/tmp, /private/tmp, $TMPDIR, or /var/folders)
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true|false|unavailable
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true|false|unavailable
  Real-store paths (e.g. ~/.local/state/irin/candidates) never honor overrides.
EOF
}

CANDIDATE_ARG=""
WANT_JSON=0
REQUIRE_TIER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate)
      [[ $# -ge 2 ]] || die_usage "--candidate requires a path"
      CANDIDATE_ARG="$2"
      shift 2
      ;;
    --json)
      WANT_JSON=1
      shift
      ;;
    --require)
      [[ $# -ge 2 ]] || die_usage "--require requires a tier"
      REQUIRE_TIER="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die_usage "unknown argument: $1"
      ;;
  esac
done

[[ -n "$CANDIDATE_ARG" ]] || die_usage "--candidate is required"
[[ "$CANDIDATE_ARG" == /* ]] || die_usage "--candidate must be an absolute path: $CANDIDATE_ARG"

case "$REQUIRE_TIER" in
  ""|"Candidate verified"|"Installed"|"Accepted"|"Published") ;;
  *) die_usage "invalid --require tier: $REQUIRE_TIER (use Candidate verified|Installed|Accepted|Published)" ;;
esac

irin_resolve_candidate_root
# Physical containment: resolve root and candidate with pwd -P so a symlink
# lexically under the store cannot escape to bytes outside it.
CAND_ROOT="$(cd "$IRIN_CANDIDATE_ROOT" && pwd -P)" \
  || die_eval "could not physically resolve IRIN_CANDIDATE_ROOT: $IRIN_CANDIDATE_ROOT"
export IRIN_CANDIDATE_ROOT="$CAND_ROOT"

[[ -d "$CANDIDATE_ARG" ]] || die_eval "candidate path is not a directory: $CANDIDATE_ARG"
CANDIDATE="$(cd "$CANDIDATE_ARG" && pwd -P)" \
  || die_eval "could not physically resolve candidate path: $CANDIDATE_ARG"
case "$CANDIDATE" in
  "$CAND_ROOT"/*) ;;
  *) die_eval "candidate path must be under IRIN_CANDIDATE_ROOT ($CAND_ROOT); got $CANDIDATE (physical)" ;;
esac
case "$CANDIDATE" in
  */failed/*) die_eval "refusing failed attempt path as candidate: $CANDIDATE" ;;
esac

# Test overrides only in hermetic temp-store mode. Real-store operation never
# consumes IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN / _CI_REQUIRED.
hermetic_overrides_allowed() {
  [[ "${IRIN_CANDIDATE_STATUS_HERMETIC:-}" == "1" ]] || return 1
  local tmp_base
  tmp_base="${TMPDIR:-/tmp}"
  # Normalize trailing slash and resolve physically when possible.
  if [[ -d "$tmp_base" ]]; then
    tmp_base="$(cd "$tmp_base" && pwd -P)" || tmp_base="${TMPDIR:-/tmp}"
  fi
  case "$CAND_ROOT" in
    /tmp/*|/private/tmp/*|"$tmp_base"/*|/var/folders/*)
      return 0
      ;;
  esac
  return 1
}

# External facts (Git main reachability + CI required aggregate).
# Unavailable never becomes green.
check_source_on_main() {
  local sha="$1" override
  override="${IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN:-}"
  if [[ -n "$override" ]] && hermetic_overrides_allowed; then
    case "$override" in
      true|false|unavailable) printf '%s' "$override"; return 0 ;;
      *) printf 'unavailable'; return 0 ;;
    esac
  fi
  # Production / non-hermetic: ignore any override and query local git.
  if ! git -C "$ROOT" rev-parse --verify -q origin/main >/dev/null 2>&1; then
    printf 'unavailable'
    return 0
  fi
  if ! git -C "$ROOT" cat-file -e "${sha}^{commit}" 2>/dev/null; then
    if ! git -C "$ROOT" fetch -q --no-tags origin "$sha" 2>/dev/null; then
      printf 'unavailable'
      return 0
    fi
  fi
  if git -C "$ROOT" merge-base --is-ancestor "$sha" origin/main 2>/dev/null; then
    printf 'true'
  else
    printf 'false'
  fi
}

check_ci_required_green() {
  local sha="$1" override owner_repo api_out run_state
  override="${IRIN_CANDIDATE_STATUS_CI_REQUIRED:-}"
  if [[ -n "$override" ]] && hermetic_overrides_allowed; then
    case "$override" in
      true|false|unavailable) printf '%s' "$override"; return 0 ;;
      *) printf 'unavailable'; return 0 ;;
    esac
  fi
  if ! command -v gh >/dev/null 2>&1; then
    printf 'unavailable'
    return 0
  fi
  owner_repo="$(git -C "$ROOT" remote get-url origin 2>/dev/null \
    | sed -E 's#.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$#\1#')" || true
  if [[ -z "$owner_repo" || "$owner_repo" == *"github.com"* ]]; then
    printf 'unavailable'
    return 0
  fi
  # Accept both the nested workflow name ("ci / CI required") and any bare
  # "CI required" context so PR tips and older commits both resolve.
  if api_out="$(gh api "repos/${owner_repo}/commits/${sha}/check-runs?per_page=100" \
      2>/dev/null)"; then
    if [[ -z "$api_out" || "$api_out" == "null" ]]; then
      printf 'unavailable'
      return 0
    fi
    if ! run_state="$(printf '%s' "$api_out" | python3 -c \
      'import json,sys; d=json.load(sys.stdin); runs=[r for r in d.get("check_runs", []) if r.get("name") in ("CI required", "ci / CI required")]; latest=max(runs, key=lambda r: (r.get("completed_at") or r.get("started_at") or "", r.get("started_at") or ""), default=None); print("" if latest is None else "{}\t{}".format(latest.get("status") or "", latest.get("conclusion") or ""))' 2>/dev/null)"; then
      printf 'unavailable'
      return 0
    fi
    case "$run_state" in
      $'completed\tsuccess') printf 'true' ;;
      "") printf 'unavailable' ;;
      *) printf 'false' ;;
    esac
    return 0
  fi
  printf 'unavailable'
}

PRE_SHA=""
if [[ -f "$CANDIDATE/candidate.json" ]]; then
  PRE_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("source_sha",""))' \
    "$CANDIDATE/candidate.json" 2>/dev/null || true)"
fi
if [[ "$PRE_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  SOURCE_ON_MAIN_HINT="$(check_source_on_main "$PRE_SHA")"
  CI_REQUIRED_HINT="$(check_ci_required_green "$PRE_SHA")"
else
  SOURCE_ON_MAIN_HINT="unavailable"
  CI_REQUIRED_HINT="unavailable"
fi

STATUS_FILE="$(mktemp)"
# shellcheck disable=SC2064 # expand STATUS_FILE now; trap needs the path value
trap 'rm -f "'"$STATUS_FILE"'"' EXIT

set +e
python3 - "$CANDIDATE" "$SOURCE_ON_MAIN_HINT" "$CI_REQUIRED_HINT" >"$STATUS_FILE" <<'PY'
import hashlib
import json
import os
import re
import sys

candidate = os.path.abspath(sys.argv[1])
source_on_main = sys.argv[2]
ci_required = sys.argv[3]

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SHA40_RE = re.compile(r"^[0-9a-f]{40}$")

blockers = []
checks = {
    "identity_ok": False,
    "payload_ok": False,
    "source_on_main": source_on_main,
    "ci_required_green": ci_required,
    "verify_proof": False,
    "install_proof": False,
    "acceptance_proof": False,
    "publication_proof": False,
}
caveats = []
candidate_id = None
source_sha = None
semver = None
pack_mode = None
dmg_sha256 = None
bundle_manifest_digest = None
well_formed = False
tier = None


def file_sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def add_blocker(code: str, message: str, blocks_tier: str) -> None:
    blockers.append({"code": code, "message": message, "blocks_tier": blocks_tier})


def load_json(path: str):
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def canonical_identity_bytes(doc: dict) -> bytes:
    return (
        json.dumps(doc, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
    ).encode("utf-8")


def parse_hashes(path: str) -> dict:
    out = {}
    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line or "=" not in line:
                continue
            k, v = line.split("=", 1)
            if k in out:
                raise ValueError(f"duplicate HASHES key: {k}")
            out[k] = v
    return out


def compute_bundle_manifest_rows(app: str) -> list:
    """Match packaging/env.sh irin_write_bundle_manifest row shape."""
    app = os.path.abspath(app)
    rows = []

    def mode_oct(path: str) -> str:
        return format(os.lstat(path).st_mode & 0o777, "04o")

    for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
        dirnames.sort()
        filenames.sort()
        for name in dirnames:
            full = os.path.join(dirpath, name)
            if os.path.islink(full):
                rel = os.path.relpath(full, app)
                rows.append(
                    (rel.replace(os.sep, "/"), "symlink", mode_oct(full), os.readlink(full))
                )
            else:
                rel = os.path.relpath(full, app)
                rows.append((rel.replace(os.sep, "/"), "dir", mode_oct(full), "-"))
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, app).replace(os.sep, "/")
            if os.path.islink(full):
                rows.append((rel, "symlink", mode_oct(full), os.readlink(full)))
            elif os.path.isfile(full):
                h = hashlib.sha256()
                with open(full, "rb") as fh:
                    for chunk in iter(lambda: fh.read(1024 * 1024), b""):
                        h.update(chunk)
                rows.append((rel, "file", mode_oct(full), h.hexdigest()))
            else:
                rows.append((rel, "other", mode_oct(full), "-"))

    rows.sort(key=lambda r: r[0])
    return rows


def compute_bundle_manifest_text(app: str) -> str:
    return "".join(
        f"{rel}\t{kind}\t{mode}\t{payload}\n"
        for rel, kind, mode, payload in compute_bundle_manifest_rows(app)
    )


def parse_bundle_manifest(text: str) -> dict:
    """relpath -> (kind, mode, payload)."""
    out = {}
    for line in text.splitlines():
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            raise ValueError(f"malformed bundle-manifest row: {line!r}")
        rel, kind, mode, payload = parts
        if rel in out:
            raise ValueError(f"duplicate bundle-manifest path: {rel}")
        out[rel] = (kind, mode, payload)
    return out


def freeze_normalized_mode(mode_str: str) -> str:
    """Normalize mode for freeze-tolerant comparison.

    Packaging freezes IRIN.app with `chmod a-w` after writing bundle-manifest,
    so write bits (00222) may be cleared without a content change. Clear only
    those write bits; preserve read and execute. Examples:
      stored 0755 → 0555; stored 0644 → 0444; current 0644 vs stored 0755 fails.
    """
    try:
        mode = int(mode_str, 8)
    except ValueError as exc:
        raise ValueError(f"invalid mode octal: {mode_str!r}") from exc
    return format(mode & ~0o222, "04o")


def app_symlink_containment_errors(app: str) -> list:
    """Every symlink under IRIN.app must resolve physically inside IRIN.app.

    Safe framework-relative links (e.g. Current -> A) pass. Absolute or
    escaping targets fail — same rule as packaging/env.sh payload assert.
    """
    errs = []
    app_real = os.path.realpath(app)
    app_prefix = app_real if app_real.endswith(os.sep) else app_real + os.sep
    for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
        for name in list(dirnames) + list(filenames):
            full = os.path.join(dirpath, name)
            if not os.path.islink(full):
                continue
            rel = os.path.relpath(full, app).replace(os.sep, "/")
            target = os.readlink(full)
            try:
                resolved = os.path.realpath(full)
            except OSError as exc:
                errs.append(f"IRIN.app symlink {rel} could not be resolved: {exc}")
                continue
            if resolved != app_real and not resolved.startswith(app_prefix):
                errs.append(
                    f"IRIN.app symlink escapes app: {rel} -> {target!r} "
                    f"(resolved {resolved})"
                )
    return errs


def app_content_matches_manifest(app: str, stored_manifest_text: str) -> list:
    """Verify current IRIN.app against a stored bundle-manifest.

    Compares path set, entry kind, content payload (file SHA-256 or symlink
    target), and freeze-normalized mode (write bits cleared; r/x preserved).
    Executable-bit loss (e.g. 0755→0644) fails; pure a-w freeze (0755→0555)
    passes. Also requires every symlink target to resolve physically inside
    IRIN.app (absolute/escaping links cannot become Candidate verified).
    """
    errs = []
    errs.extend(app_symlink_containment_errors(app))
    try:
        stored = parse_bundle_manifest(stored_manifest_text)
    except ValueError as exc:
        return errs + [str(exc)]
    current_rows = compute_bundle_manifest_rows(app)
    current = {rel: (kind, mode, payload) for rel, kind, mode, payload in current_rows}
    stored_paths = set(stored)
    current_paths = set(current)
    missing = sorted(stored_paths - current_paths)
    extra = sorted(current_paths - stored_paths)
    if missing:
        errs.append(f"IRIN.app missing paths from bundle-manifest: {missing[:5]}")
    if extra:
        errs.append(f"IRIN.app has paths not in bundle-manifest: {extra[:5]}")
    for rel in sorted(stored_paths & current_paths):
        s_kind, s_mode, s_payload = stored[rel]
        c_kind, c_mode, c_payload = current[rel]
        if s_kind != c_kind:
            errs.append(f"IRIN.app kind mismatch for {rel}: stored={s_kind} current={c_kind}")
            continue
        if s_payload != c_payload:
            errs.append(f"IRIN.app content mismatch for {rel}")
        try:
            s_norm = freeze_normalized_mode(s_mode)
            c_norm = freeze_normalized_mode(c_mode)
        except ValueError as exc:
            errs.append(f"IRIN.app mode parse for {rel}: {exc}")
            continue
        if s_norm != c_norm:
            errs.append(
                f"IRIN.app mode mismatch for {rel}: "
                f"stored={s_mode} (freeze-norm {s_norm}) "
                f"current={c_mode} (freeze-norm {c_norm})"
            )
    return errs


def proof_core_ok(doc: dict, kind: str, cid: str, sha: str) -> list:
    errs = []
    if doc.get("schema_version") != 1:
        errs.append("schema_version must be 1")
    if doc.get("proof_kind") != kind:
        errs.append(f"proof_kind must be {kind!r}")
    if doc.get("candidate_id") != cid:
        errs.append("candidate_id does not match store identity")
    if doc.get("source_sha") != sha:
        errs.append("source_sha does not match candidate identity")
    if not doc.get("run_id"):
        errs.append("run_id missing")
    if not doc.get("timestamp"):
        errs.append("timestamp missing")
    if not doc.get("tool_version"):
        errs.append("tool_version missing")
    return errs


def require_result_pass(doc: dict) -> list:
    if doc.get("result") != "PASS":
        return [f"result is {doc.get('result')!r}, not PASS"]
    return []


def expiry_unexpired(expiry: str) -> list:
    """Reject missing/unparseable/expired authorization expiry (UTC)."""
    from datetime import datetime, timezone

    if not expiry or not isinstance(expiry, str):
        return ["expiry missing"]
    raw = expiry.strip()
    try:
        if raw.endswith("Z"):
            exp = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        else:
            exp = datetime.fromisoformat(raw)
        if exp.tzinfo is None:
            exp = exp.replace(tzinfo=timezone.utc)
    except ValueError:
        return [f"expiry is not a parseable timestamp: {expiry!r}"]
    now = datetime.now(timezone.utc)
    if exp < now:
        return [f"authorization expired at {expiry}"]
    return []


cand_json_path = os.path.join(candidate, "candidate.json")
hashes_path = os.path.join(candidate, "HASHES.txt")
bm_path = os.path.join(candidate, "bundle-manifest.txt")
app_path = os.path.join(candidate, "IRIN.app")

if not os.path.isfile(cand_json_path):
    add_blocker("missing_candidate_json", "candidate.json missing", "Candidate verified")
elif not os.path.isfile(hashes_path):
    add_blocker("missing_hashes", "HASHES.txt missing", "Candidate verified")
elif not os.path.isfile(bm_path):
    add_blocker("missing_bundle_manifest", "bundle-manifest.txt missing", "Candidate verified")
elif not os.path.isdir(app_path):
    add_blocker("missing_app", "IRIN.app missing", "Candidate verified")
else:
    dmgs = sorted(
        n
        for n in os.listdir(candidate)
        if n.endswith(".dmg") and os.path.isfile(os.path.join(candidate, n))
    )
    if len(dmgs) != 1:
        add_blocker(
            "dmg_count",
            f"candidate must contain exactly one DMG (found {len(dmgs)})",
            "Candidate verified",
        )
    else:
        try:
            raw = open(cand_json_path, "rb").read()
            identity = json.loads(raw.decode("utf-8"))
            if not isinstance(identity, dict):
                raise ValueError("candidate.json must be an object")
            canon = canonical_identity_bytes(identity)
            if raw != canon:
                add_blocker(
                    "identity_not_canonical",
                    "candidate.json is not in canonical identity form",
                    "Candidate verified",
                )
            recomputed_id = hashlib.sha256(raw).hexdigest()
            expected_id = os.path.basename(candidate)
            candidate_id = recomputed_id
            if recomputed_id != expected_id:
                add_blocker(
                    "candidate_id_mismatch",
                    f"candidate-id does not recompute from candidate.json "
                    f"(store={expected_id} recomputed={recomputed_id})",
                    "Candidate verified",
                )
            source_sha = identity.get("source_sha")
            semver = identity.get("semver")
            pack_mode = identity.get("pack_mode")
            dmg_sha256 = identity.get("dmg_sha256")
            bundle_manifest_digest = identity.get("bundle_manifest_digest")
            required_keys = {
                "schema_version",
                "source_sha",
                "semver",
                "pack_mode",
                "bundle_manifest_digest",
                "dmg_sha256",
                "stapled",
                "gateway_digest",
                "sidecar_digest",
            }
            missing = sorted(required_keys - set(identity.keys()))
            if missing:
                add_blocker(
                    "identity_fields",
                    f"candidate.json missing fields: {', '.join(missing)}",
                    "Candidate verified",
                )
            if identity.get("schema_version") != 1:
                add_blocker(
                    "identity_schema",
                    "candidate.json schema_version must be 1",
                    "Candidate verified",
                )
            if not isinstance(source_sha, str) or not SHA40_RE.fullmatch(source_sha):
                add_blocker(
                    "identity_source_sha",
                    "source_sha must be 40-char lowercase hex",
                    "Candidate verified",
                )
            if not isinstance(dmg_sha256, str) or not SHA256_RE.fullmatch(dmg_sha256):
                add_blocker(
                    "identity_dmg_sha",
                    "dmg_sha256 must be 64-char lowercase hex",
                    "Candidate verified",
                )
            if not isinstance(bundle_manifest_digest, str) or not SHA256_RE.fullmatch(
                bundle_manifest_digest
            ):
                add_blocker(
                    "identity_bundle_digest",
                    "bundle_manifest_digest must be 64-char lowercase hex",
                    "Candidate verified",
                )
            if pack_mode not in ("local-dev", "signed-rc", "production"):
                add_blocker(
                    "identity_pack_mode",
                    f"pack_mode must be local-dev|signed-rc|production (got {pack_mode!r})",
                    "Candidate verified",
                )

            hashes = parse_hashes(hashes_path)
            mismatches = []
            if hashes.get("source_sha") != source_sha:
                mismatches.append("HASHES source_sha != identity")
            if hashes.get("dmg_sha256") != dmg_sha256:
                mismatches.append("HASHES dmg_sha256 != identity")
            if hashes.get("bundle_manifest_digest") != bundle_manifest_digest:
                mismatches.append("HASHES bundle_manifest_digest != identity")
            if hashes.get("pack_mode") != pack_mode:
                mismatches.append("HASHES pack_mode != identity")
            if hashes.get("gateway_digest") != identity.get("gateway_digest"):
                mismatches.append("HASHES gateway_digest != identity")
            if hashes.get("sidecar_digest") != identity.get("sidecar_digest"):
                mismatches.append("HASHES sidecar_digest != identity")

            actual_dmg = file_sha256(os.path.join(candidate, dmgs[0]))
            if actual_dmg != dmg_sha256:
                mismatches.append("DMG bytes do not match identity dmg_sha256")
            actual_bm = file_sha256(bm_path)
            if actual_bm != bundle_manifest_digest:
                mismatches.append("bundle-manifest.txt does not match identity digest")

            # Content-identity of current IRIN.app vs stored bundle-manifest
            # (path/kind/payload; modes ignored — freeze uses chmod a-w).
            stored_bm_text = open(bm_path, "r", encoding="utf-8").read()
            for err in app_content_matches_manifest(app_path, stored_bm_text):
                mismatches.append(err)

            identity_blocker_codes = {
                b["code"]
                for b in blockers
                if b["code"].startswith("identity_")
                or b["code"] in ("candidate_id_mismatch", "identity_not_canonical")
            }
            checks["identity_ok"] = not identity_blocker_codes
            # well_formed = structural identity (reportable candidate). Payload
            # integrity is separate: app/DMG mutation blocks tiers, not reporting.
            well_formed = checks["identity_ok"]
            if mismatches:
                for m in mismatches:
                    add_blocker("payload_hash_mismatch", m, "Candidate verified")
                checks["payload_ok"] = False
            else:
                checks["payload_ok"] = checks["identity_ok"]
        except Exception as exc:  # noqa: BLE001
            add_blocker(
                "identity_parse",
                f"could not parse identity/payload: {exc}",
                "Candidate verified",
            )

if well_formed:
    if source_on_main == "false":
        add_blocker(
            "source_not_on_main",
            "source SHA is not reachable from fetched origin/main",
            "Candidate verified",
        )
    elif source_on_main != "true":
        add_blocker(
            "source_on_main_unavailable",
            "could not determine whether source SHA is on origin/main "
            "(network/local refs unavailable)",
            "Candidate verified",
        )
    if ci_required == "false":
        add_blocker(
            "ci_required_not_green",
            "GitHub CI required aggregate is not green for this source SHA",
            "Candidate verified",
        )
    elif ci_required != "true":
        add_blocker(
            "ci_required_unavailable",
            "could not determine CI required aggregate state (network/auth unavailable)",
            "Candidate verified",
        )

proofs_dir = os.path.join(candidate, "proofs")


def load_proof(name: str):
    path = os.path.join(proofs_dir, name)
    if not os.path.isfile(path):
        return None, path
    try:
        return load_json(path), path
    except Exception as exc:  # noqa: BLE001
        add_blocker(
            f"{name}_parse",
            f"{name} is not valid JSON: {exc}",
            "Candidate verified",
        )
        return None, path


if well_formed and candidate_id and source_sha:
    verify_doc, verify_path = load_proof("verify.json")
    verify_ok = False
    if verify_doc is None:
        if not os.path.isfile(verify_path):
            add_blocker(
                "missing_verify_proof",
                "proofs/verify.json missing",
                "Candidate verified",
            )
    else:
        errs = proof_core_ok(verify_doc, "verify", candidate_id, source_sha) + require_result_pass(
            verify_doc
        )
        # DMG + bundle digests are required relevant bindings, not optional extras.
        if not verify_doc.get("dmg_sha256"):
            errs.append("verify proof dmg_sha256 missing")
        elif verify_doc.get("dmg_sha256") != dmg_sha256:
            errs.append("verify proof dmg_sha256 does not match candidate identity")
        if not verify_doc.get("bundle_manifest_digest"):
            errs.append("verify proof bundle_manifest_digest missing")
        elif verify_doc.get("bundle_manifest_digest") != bundle_manifest_digest:
            errs.append(
                "verify proof bundle_manifest_digest does not match candidate identity"
            )
        if errs:
            for e in errs:
                add_blocker("verify_proof_invalid", e, "Candidate verified")
        else:
            verify_ok = True
            checks["verify_proof"] = True

    can_verified = (
        verify_ok
        and source_on_main == "true"
        and ci_required == "true"
        and checks["identity_ok"]
        and checks["payload_ok"]
    )
    if can_verified:
        tier = "Candidate verified"

    install_doc, _install_path = load_proof("install.json")
    install_ok = False
    if install_doc is not None:
        errs = proof_core_ok(install_doc, "install", candidate_id, source_sha) + require_result_pass(
            install_doc
        )
        cand_bm = install_doc.get("candidate_bundle_manifest_digest")
        inst_bm = install_doc.get("installed_bundle_manifest_digest")
        if not cand_bm:
            errs.append("install proof candidate_bundle_manifest_digest missing")
        elif cand_bm != bundle_manifest_digest:
            errs.append(
                "install proof candidate_bundle_manifest_digest does not match identity"
            )
        if not inst_bm:
            errs.append("install proof installed_bundle_manifest_digest missing")
        elif inst_bm != bundle_manifest_digest:
            errs.append(
                "install proof installed_bundle_manifest_digest does not match identity"
            )
        if cand_bm and inst_bm and cand_bm != inst_bm:
            errs.append("install proof candidate vs installed digests diverge")
        install_root = os.path.join(candidate, "install")
        inst_app = os.path.join(install_root, "IRIN.app")
        inst_bm_path = os.path.join(install_root, "bundle-manifest.txt")
        if not os.path.isdir(install_root):
            errs.append("install/ directory missing")
        else:
            if not os.path.isfile(inst_bm_path):
                errs.append("install/bundle-manifest.txt missing")
            if not os.path.isdir(inst_app):
                errs.append("install/IRIN.app missing")
            if os.path.isfile(inst_bm_path) and os.path.isdir(inst_app):
                stored_inst_bytes = open(inst_bm_path, "rb").read()
                stored_inst_text = stored_inst_bytes.decode("utf-8")
                for err in app_content_matches_manifest(inst_app, stored_inst_text):
                    errs.append(f"install: {err}")
                actual_digest = hashlib.sha256(stored_inst_bytes).hexdigest()
                if inst_bm and actual_digest != inst_bm:
                    errs.append(
                        "install/bundle-manifest.txt does not match install proof digest"
                    )
                if actual_digest != bundle_manifest_digest:
                    errs.append(
                        "installed bundle-manifest digest does not match candidate identity"
                    )
                # Content identity vs candidate (path/kind/payload + freeze-norm mode).
                cand_bm_text = open(bm_path, "r", encoding="utf-8").read()
                try:
                    cand_map = parse_bundle_manifest(cand_bm_text)
                    inst_map = parse_bundle_manifest(stored_inst_text)

                    def content_key(m: dict) -> dict:
                        out = {}
                        for p, (k, mode, pl) in m.items():
                            out[p] = (k, freeze_normalized_mode(mode), pl)
                        return out

                    if content_key(cand_map) != content_key(inst_map):
                        errs.append(
                            "install bundle-manifest content identity differs from candidate"
                        )
                except ValueError as exc:
                    errs.append(f"install manifest parse: {exc}")
        if errs:
            for e in errs:
                add_blocker("install_proof_invalid", e, "Installed")
        else:
            install_ok = True
            checks["install_proof"] = True
    elif can_verified:
        add_blocker("missing_install_proof", "proofs/install.json missing", "Installed")

    if can_verified and install_ok:
        tier = "Installed"

    acceptance_doc, acceptance_path = load_proof("acceptance.json")
    t2_doc, t2_path = load_proof("t2.json")
    acceptance_ok = False
    if acceptance_doc is not None or t2_doc is not None or (can_verified and install_ok):
        acc_errs = []
        if acceptance_doc is None:
            if not os.path.isfile(acceptance_path):
                acc_errs.append("proofs/acceptance.json missing")
        else:
            acc_errs.extend(
                proof_core_ok(acceptance_doc, "acceptance", candidate_id, source_sha)
            )
            acc_errs.extend(require_result_pass(acceptance_doc))
            if not acceptance_doc.get("dmg_sha256"):
                acc_errs.append("acceptance dmg_sha256 missing")
            elif acceptance_doc.get("dmg_sha256") != dmg_sha256:
                acc_errs.append(
                    "acceptance dmg_sha256 does not match current candidate DMG hash"
                )
            if not acceptance_doc.get("installed_bundle_manifest_digest"):
                acc_errs.append("acceptance installed_bundle_manifest_digest missing")
            elif acceptance_doc.get("installed_bundle_manifest_digest") != bundle_manifest_digest:
                acc_errs.append(
                    "acceptance installed_bundle_manifest_digest does not match "
                    "current candidate digest"
                )
            if not acceptance_doc.get("pending_action_id"):
                acc_errs.append("acceptance pending_action_id missing")

        t2_errs = []
        if t2_doc is None:
            if not os.path.isfile(t2_path):
                t2_errs.append("proofs/t2.json missing")
        else:
            # Same schema-valid envelope as other tier proofs: core + PASS + source.
            t2_errs.extend(proof_core_ok(t2_doc, "t2", candidate_id, source_sha))
            t2_errs.extend(require_result_pass(t2_doc))
            if not t2_doc.get("action_id"):
                t2_errs.append("t2 action_id missing")
            if not t2_doc.get("acceptance_digest") or not SHA256_RE.fullmatch(
                str(t2_doc.get("acceptance_digest"))
            ):
                t2_errs.append("t2 acceptance_digest must be 64-char hex")
            if not t2_doc.get("authorized_effects"):
                t2_errs.append("t2 authorized_effects missing")
            t2_errs.extend(expiry_unexpired(t2_doc.get("expiry")))

        if acceptance_doc is not None and t2_doc is not None and not acc_errs:
            acc_digest = file_sha256(acceptance_path)
            if t2_doc.get("acceptance_digest") != acc_digest:
                t2_errs.append(
                    "t2 acceptance_digest does not match proofs/acceptance.json bytes"
                )
            if acceptance_doc.get("pending_action_id") and t2_doc.get("action_id"):
                if acceptance_doc.get("pending_action_id") != t2_doc.get("action_id"):
                    t2_errs.append(
                        "t2 action_id does not match acceptance pending_action_id"
                    )

        if acc_errs or t2_errs:
            for e in acc_errs:
                add_blocker("acceptance_invalid", e, "Accepted")
            for e in t2_errs:
                add_blocker("t2_invalid", e, "Accepted")
        elif acceptance_doc is not None and t2_doc is not None:
            acceptance_ok = True
            checks["acceptance_proof"] = True
            caveats.append(
                "Accepted does not cryptographically prove who typed a structurally "
                "valid receipt; the human boundary is the operator-controlled T2 action"
            )

    if tier == "Installed" and acceptance_ok:
        tier = "Accepted"

    pub_doc, _pub_path = load_proof("publication.json")
    pub_ok = False
    if pub_doc is not None:
        errs = proof_core_ok(
            pub_doc, "publication", candidate_id, source_sha
        ) + require_result_pass(pub_doc)
        if pub_doc.get("public_state") not in ("published", "public"):
            errs.append("publication public_state must be 'published' (or 'public')")
        if pub_doc.get("redownload_unauthenticated") is not True:
            errs.append("publication must record redownload_unauthenticated=true")
        asset_sha = pub_doc.get("asset_sha256") or pub_doc.get("dmg_sha256")
        if asset_sha != dmg_sha256:
            errs.append(
                "publication asset hash does not match candidate post-staple DMG hash"
            )
        if not pub_doc.get("release_url") and not pub_doc.get("tag"):
            errs.append("publication must name release_url or tag")
        if errs:
            for e in errs:
                add_blocker("publication_invalid", e, "Published")
        else:
            pub_ok = True
            checks["publication_proof"] = True
    elif tier == "Accepted":
        add_blocker(
            "missing_publication_proof",
            "proofs/publication.json missing",
            "Published",
        )

    if tier == "Accepted" and pub_ok:
        tier = "Published"

status = {
    "schema_version": 1,
    "reporter": "scripts/candidate-status.sh",
    "candidate_path": candidate,
    "candidate_id": candidate_id,
    "source_sha": source_sha,
    "semver": semver,
    "pack_mode": pack_mode,
    "dmg_sha256": dmg_sha256,
    "bundle_manifest_digest": bundle_manifest_digest,
    "well_formed": well_formed,
    "tier": tier,
    "blockers": blockers,
    "checks": checks,
    "caveats": caveats,
}
json.dump(status, sys.stdout, sort_keys=True, indent=2)
sys.stdout.write("\n")
sys.exit(0 if well_formed else 3)
PY
PY_EC=$?
set -e

if [[ ! -s "$STATUS_FILE" ]]; then
  die_eval "candidate-status produced no status document (python exit $PY_EC)"
fi

WELL_FORMED="$(python3 -c 'import json,sys; print("true" if json.load(open(sys.argv[1])).get("well_formed") else "false")' "$STATUS_FILE")"
TIER="$(python3 -c 'import json,sys; t=json.load(open(sys.argv[1])).get("tier"); print(t if t else "")' "$STATUS_FILE")"
BLOCKERS_TEXT="$(python3 - "$STATUS_FILE" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
for b in d.get("blockers") or []:
    print(f"  - [{b.get('blocks_tier')}] {b.get('code')}: {b.get('message')}")
PY
)"

if [[ "$WANT_JSON" == "1" ]]; then
  cat "$STATUS_FILE"
else
  printf 'candidate: %s\n' "$CANDIDATE"
  printf 'candidate_id: %s\n' "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("candidate_id") or "")' "$STATUS_FILE")"
  printf 'source_sha: %s\n' "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("source_sha") or "")' "$STATUS_FILE")"
  printf 'pack_mode: %s\n' "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("pack_mode") or "")' "$STATUS_FILE")"
  if [[ -n "$TIER" ]]; then
    printf 'tier: %s\n' "$TIER"
  else
    printf 'tier: (none — below Candidate verified)\n'
  fi
  if [[ -n "$BLOCKERS_TEXT" ]]; then
    printf 'blockers:\n%s\n' "$BLOCKERS_TEXT"
  else
    printf 'blockers: (none)\n'
  fi
  CAVEATS="$(python3 -c 'import json,sys; print("\n".join(json.load(open(sys.argv[1])).get("caveats") or []))' "$STATUS_FILE")"
  if [[ -n "$CAVEATS" ]]; then
    printf 'caveats:\n'
    while IFS= read -r line; do
      [[ -n "$line" ]] && printf '  - %s\n' "$line"
    done <<<"$CAVEATS"
  fi
fi

if [[ "$WELL_FORMED" != "true" ]]; then
  exit 2
fi

if [[ -n "$REQUIRE_TIER" ]]; then
  MEETS="$(python3 - "$STATUS_FILE" "$REQUIRE_TIER" <<'PY'
import json, sys
order = [None, "Candidate verified", "Installed", "Accepted", "Published"]
rank = {t: i for i, t in enumerate(order)}
d = json.load(open(sys.argv[1]))
need = sys.argv[2]
have = d.get("tier")
print("yes" if rank.get(have, 0) >= rank[need] else "no")
PY
)"
  if [[ "$MEETS" != "yes" ]]; then
    exit 1
  fi
fi

exit 0
