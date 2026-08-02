#!/usr/bin/env bash
# record-acceptance.sh — final-production acceptance part of T2 (not T1).
#
# Interactive only (real tty) for a fresh acceptance phrase.
# Crash resume: if acceptance.json exists, t2.json is missing, and pending-t2
# still matches, validate the existing acceptance and write only t2.json
# (no rewrite of acceptance; no second phrase required for resume).
#
# Usage:
#   scripts/record-acceptance.sh \
#     --candidate ABSOLUTE_STORE_PATH \
#     --installed-app ABSOLUTE_APP_PATH
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

CANDIDATE_ARG=""
INSTALLED_APP=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate) CANDIDATE_ARG="${2:-}"; shift 2 ;;
    --installed-app) INSTALLED_APP="${2:-}"; shift 2 ;;
    -h|--help)
      cat <<'EOF'
Usage: record-acceptance.sh --candidate ABSOLUTE_STORE_PATH --installed-app ABSOLUTE_APP_PATH

Final-production acceptance (T2). Requires a real tty for a fresh phrase.
If acceptance.json already exists without t2.json, resumes by validating the
existing acceptance against pending-t2 and writing only t2.json.
EOF
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$CANDIDATE_ARG" ]] || die "usage: $0 --candidate PATH --installed-app PATH"
[[ -n "$INSTALLED_APP" ]] || die "usage: $0 --candidate PATH --installed-app PATH"
export IRIN_CANDIDATE_PATH="$CANDIDATE_ARG"
irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"

[[ "$INSTALLED_APP" == /* ]] || die "--installed-app must be absolute: $INSTALLED_APP"
[[ -d "$INSTALLED_APP" ]] || die "installed app missing: $INSTALLED_APP"
case "$(basename "$INSTALLED_APP")" in
  IRIN.app) ;;
  *) die "installed app must be named IRIN.app (got $(basename "$INSTALLED_APP"))" ;;
esac

CANDIDATE_ID="$(basename "$CANDIDATE")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] || die "bad candidate-id path: $CANDIDATE"

eval "$(python3 - "$CANDIDATE/candidate.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
for k in ("source_sha", "semver", "pack_mode", "dmg_sha256", "bundle_manifest_digest"):
    if k not in d or d[k] is None:
        raise SystemExit(f"candidate.json missing {k}")
print(f'SOURCE_SHA={json.dumps(d["source_sha"])}')
print(f'PACK_MODE={json.dumps(d["pack_mode"])}')
print(f'DMG_SHA256={json.dumps(d["dmg_sha256"])}')
print(f'BUNDLE_MANIFEST_DIGEST={json.dumps(d["bundle_manifest_digest"])}')
print(f'STAPLED={json.dumps("true" if d.get("stapled") else "false")}')
PY
)"

[[ "$PACK_MODE" == "production" ]] || die "acceptance is for production candidates only (pack_mode=$PACK_MODE)"
[[ "$STAPLED" == "true" ]] || die "acceptance requires stapled=true production candidate"

PENDING="$CANDIDATE/proofs/pending-t2.json"
ACC_PATH="$CANDIDATE/proofs/acceptance.json"
T2_PATH="$CANDIDATE/proofs/t2.json"

[[ -f "$T2_PATH" ]] && die "proofs/t2.json already exists; refusing to rewrite"

RESUME=0
if [[ -f "$ACC_PATH" ]]; then
  # Crash window: acceptance written, t2 not yet.
  [[ -f "$PENDING" ]] || die "acceptance.json exists without pending-t2.json and without t2.json; cannot resume safely"
  RESUME=1
  note "resume mode: acceptance.json present, completing matching T2 only"
else
  [[ -f "$PENDING" ]] || die "missing pending T2 packet: $PENDING (board must create it first)"
fi

eval "$(python3 - "$PENDING" "$CANDIDATE_ID" <<'PY'
import json, sys
from datetime import datetime, timezone
path, cid = sys.argv[1], sys.argv[2]
d = json.load(open(path))
if d.get("schema_version") != 1:
    raise SystemExit("pending-t2 schema_version must be 1")
if d.get("packet_kind") != "pending-t2":
    raise SystemExit("pending-t2 packet_kind must be 'pending-t2'")
if d.get("candidate_id") != cid:
    raise SystemExit("pending-t2 candidate_id does not match candidate path")
action_id = d.get("action_id")
if not action_id or not isinstance(action_id, str):
    raise SystemExit("pending-t2 action_id missing")
effects = d.get("authorized_effects")
if not isinstance(effects, list) or not effects:
    raise SystemExit("pending-t2 authorized_effects must be a non-empty list")
required = {"tag-push", "release-attach", "publish", "version-image-labels"}
missing = sorted(required - set(effects))
if missing:
    raise SystemExit(f"pending-t2 authorized_effects missing: {', '.join(missing)}")
expiry = d.get("expiry")
if not expiry:
    raise SystemExit("pending-t2 expiry missing")
raw = str(expiry).strip()
if raw.endswith("Z"):
    exp = datetime.fromisoformat(raw.replace("Z", "+00:00"))
else:
    exp = datetime.fromisoformat(raw)
if exp.tzinfo is None:
    exp = exp.replace(tzinfo=timezone.utc)
if exp < datetime.now(timezone.utc):
    raise SystemExit(f"pending-t2 authorization expired at {expiry}")
print(f'ACTION_ID={json.dumps(action_id)}')
print(f'EXPIRY={json.dumps(str(expiry))}')
print(f"EFFECTS_JSON={json.dumps(json.dumps(effects))}")
PY
)"

[[ -f "$CANDIDATE/proofs/install.json" ]] \
  || die "proofs/install.json missing — run install-verify-candidate.sh first"
[[ -d "$CANDIDATE/install/IRIN.app" ]] \
  || die "candidate install/IRIN.app missing — run install-verify-candidate.sh first"

note "recompute installed app manifest (bytes Dave exercised)"
TMP_BM="$(mktemp)"
trap 'rm -f "$TMP_BM"' EXIT
irin_write_bundle_manifest "$INSTALLED_APP" "$TMP_BM"
INST_DIGEST="$(irin_sha256_file "$TMP_BM")"
[[ "$INST_DIGEST" == "$BUNDLE_MANIFEST_DIGEST" ]] \
  || die "installed-app digest mismatch: recomputed=$INST_DIGEST candidate=$BUNDLE_MANIFEST_DIGEST"

INST_CANON="$(cd "$INSTALLED_APP" && pwd)"
CAND_INSTALL_APP="$(cd "$CANDIDATE/install/IRIN.app" 2>/dev/null && pwd || true)"
if [[ -n "$CAND_INSTALL_APP" && "$INST_CANON" != "$CAND_INSTALL_APP" ]]; then
  note "installed-app path differs from candidate/install (digest match required and held)"
fi

if [[ "$RESUME" == "1" ]]; then
  # Validate full proof envelope + bindings; never rewrite acceptance.
  python3 - "$ACC_PATH" "$CANDIDATE_ID" "$SOURCE_SHA" "$DMG_SHA256" \
    "$BUNDLE_MANIFEST_DIGEST" "$ACTION_ID" <<'PY' || die "existing acceptance.json does not match resume prerequisites"
import json, sys, re
path, cid, sha, dmg, bm, action = sys.argv[1:]
d = json.load(open(path))
errs = []
if d.get("schema_version") != 1:
    errs.append("schema_version must be 1")
if d.get("proof_kind") != "acceptance":
    errs.append("proof_kind must be acceptance")
if d.get("result") != "PASS":
    errs.append("result must be PASS")
if d.get("candidate_id") != cid:
    errs.append("candidate_id mismatch")
if d.get("source_sha") != sha:
    errs.append("source_sha mismatch")
if d.get("dmg_sha256") != dmg:
    errs.append("dmg_sha256 mismatch")
if d.get("installed_bundle_manifest_digest") != bm:
    errs.append("installed_bundle_manifest_digest mismatch")
if d.get("pending_action_id") != action:
    errs.append("pending_action_id mismatch")
# Full envelope required so resume cannot mint T2 from a stripped file that
# would leave the candidate below Accepted after pending-t2 is deleted.
if not d.get("tool_version"):
    errs.append("tool_version missing")
if not d.get("run_id"):
    errs.append("run_id missing")
if not d.get("timestamp"):
    errs.append("timestamp missing")
if errs:
    raise SystemExit("acceptance mismatch: " + "; ".join(errs))
print("acceptance_ok")
PY
  ACC_DIGEST="$(irin_sha256_file "$ACC_PATH")"
  note "existing acceptance validated (full envelope); writing t2.json only"
else
  # Fresh acceptance requires interactive tty.
  [[ -t 0 && -t 1 ]] || die "record-acceptance requires an interactive tty (stdin and stdout); piped/non-tty refused"

  echo
  echo "================================================================"
  echo " T2 final-production acceptance"
  echo " candidate:  $CANDIDATE"
  echo " candidate_id: $CANDIDATE_ID"
  echo " source_sha: $SOURCE_SHA"
  echo " dmg_sha256: $DMG_SHA256"
  echo " installed_bundle_manifest_digest: $BUNDLE_MANIFEST_DIGEST"
  echo " installed_app: $INST_CANON"
  echo " pending_action_id: $ACTION_ID"
  echo " effects: $EFFECTS_JSON"
  echo " expiry: $EXPIRY"
  echo "================================================================"
  echo
  echo "Type a confirmation phrase that includes ALL THREE of:"
  echo "  1) full source SHA"
  echo "  2) final post-staple DMG hash"
  echo "  3) installed bundle-manifest digest"
  echo
  printf 'phrase> '
  IFS= read -r PHRASE || die "failed to read confirmation phrase"
  [[ -n "${PHRASE// }" ]] || die "empty confirmation phrase refused"

  python3 - "$PHRASE" "$SOURCE_SHA" "$DMG_SHA256" "$BUNDLE_MANIFEST_DIGEST" <<'PY' \
    || die "confirmation phrase must include full source SHA + DMG hash + installed bundle-manifest digest"
import sys
phrase, sha, dmg, bm = sys.argv[1:]
missing = []
if sha not in phrase:
    missing.append("source_sha")
if dmg not in phrase:
    missing.append("dmg_sha256")
if bm not in phrase:
    missing.append("installed_bundle_manifest_digest")
if missing:
    raise SystemExit("missing: " + ", ".join(missing))
PY

  note "write proofs/acceptance.json"
  # Paths/IDs via env — never interpolate into an unquoted Python string.
  ACC_EXTRA="$(
    DMG_SHA256="$DMG_SHA256" \
    BUNDLE_MANIFEST_DIGEST="$BUNDLE_MANIFEST_DIGEST" \
    ACTION_ID="$ACTION_ID" \
    INST_CANON="$INST_CANON" \
    python3 - <<'PY'
import json, os
print(json.dumps({
  "dmg_sha256": os.environ["DMG_SHA256"],
  "installed_bundle_manifest_digest": os.environ["BUNDLE_MANIFEST_DIGEST"],
  "pending_action_id": os.environ["ACTION_ID"],
  "installed_app_path": os.environ["INST_CANON"],
  "confirmation_contains": ["source_sha", "dmg_sha256", "installed_bundle_manifest_digest"],
}))
PY
  )"
  irin_write_proof_envelope \
    "$ACC_PATH" \
    "acceptance" \
    "$CANDIDATE_ID" \
    "$SOURCE_SHA" \
    "PASS" \
    "$ACC_EXTRA"
  ACC_DIGEST="$(irin_sha256_file "$ACC_PATH")"
fi

note "complete T2 once: write proofs/t2.json"
T2_EXTRA="$(
  ACTION_ID="$ACTION_ID" ACC_DIGEST="$ACC_DIGEST" EFFECTS_JSON="$EFFECTS_JSON" EXPIRY="$EXPIRY" \
  python3 - <<'PY'
import json, os
print(json.dumps({
  "action_id": os.environ["ACTION_ID"],
  "acceptance_digest": os.environ["ACC_DIGEST"],
  "authorized_effects": json.loads(os.environ["EFFECTS_JSON"]),
  "expiry": os.environ["EXPIRY"],
}))
PY
)"
irin_write_proof_envelope \
  "$T2_PATH" \
  "t2" \
  "$CANDIDATE_ID" \
  "$SOURCE_SHA" \
  "PASS" \
  "$T2_EXTRA"

rm -f "$PENDING"

echo
echo "acceptance: $ACC_PATH"
echo "t2 written: $T2_PATH"
echo "acceptance_digest: $ACC_DIGEST"
if [[ "$RESUME" == "1" ]]; then
  echo "resume: completed T2 from existing acceptance (acceptance bytes not rewritten)"
fi
echo
echo "CAVEAT: Accepted does not cryptographically prove who typed a structurally"
echo "valid receipt. The human boundary is the operator-controlled T2 action."
echo "Status remains Installed unless candidate-status derives Accepted from this chain."
