#!/usr/bin/env bash
# W5 hermetic fake-gh publication path.
# Zero network, zero Apple, zero provider. Drives real do_publish under
# IRIN_PUBLISH_HERMETIC=1 with stateful fake gh/docker/curl on PATH.
#
# Proves:
#   - different existing draft asset refuses (no clobber)
#   - equal draft asset is idempotent (skip upload)
#   - no --clobber on gh release upload
#   - draft re-download alone does not write publication proof
#   - post-public unauthenticated hash match writes publication.json
#   - public retry finishes proof without mutation
#   - draft existence alone without workflow success refuses
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-w5-fake-gh.XXXXXX")"
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

TX="$ROOT/scripts/release-transaction.sh"
STATUS="$ROOT/scripts/candidate-status.sh"
[[ -x "$TX" && -x "$STATUS" ]] || fail "publish scripts not executable"

SEMVER="9.9.9"
TAG="v${SEMVER}"
SHA="$(python3 -c "print(('c' * 40)[:40])")"
GW_D="g$(printf '0%.0s' {1..63})"
SC_D="s$(printf '0%.0s' {1..63})"

make_staging() {
  local staging="$1" dmg_body="$2"
  rm -rf "$staging"
  mkdir -p "$staging/IRIN.app/Contents/MacOS" \
    "$staging/proofs" "$staging/smoke" "$staging/install" "$staging/logs"
  printf 'host' >"$staging/IRIN.app/Contents/MacOS/council-warroom-tauri"
  printf 'side' >"$staging/IRIN.app/Contents/MacOS/council"
  local dmg_name="IRIN_${SEMVER}_aarch64.dmg"
  printf '%s' "$dmg_body" >"$staging/$dmg_name"
  irin_write_bundle_manifest "$staging/IRIN.app" "$staging/bundle-manifest.txt"
  local bm_d dmg_d app_d
  bm_d="$(irin_sha256_file "$staging/bundle-manifest.txt")"
  dmg_d="$(irin_sha256_file "$staging/$dmg_name")"
  app_d="$(irin_sha256_file "$staging/IRIN.app/Contents/MacOS/council-warroom-tauri")"
  cat >"$staging/HASHES.txt" <<EOF
pack_mode=production
release_version=$SEMVER
releasable=true
stapled=true
source_sha=$SHA
build_dirty=false
arch=aarch64-apple-darwin
app=IRIN.app
dmg=$dmg_name
app_sha256=$app_d
council_sha256=$(irin_sha256_file "$staging/IRIN.app/Contents/MacOS/council")
arm_attest_sha256=$(printf 'x' | irin_sha256_bytes)
gateway_pack_compose_sha256=$(printf 'y' | irin_sha256_bytes)
gateway_pack_manifest_sha256=$(printf 'z' | irin_sha256_bytes)
gateway_digest=$GW_D
sidecar_digest=$SC_D
warroom_web_index_sha256=$(printf 'w' | irin_sha256_bytes)
bundle_manifest_digest=$bm_d
dmg_sha256=$dmg_d
EOF
  python3 - "$staging/candidate.json" "$SHA" "$bm_d" "$dmg_d" "$GW_D" "$SC_D" "$SEMVER" <<'PY'
import json, sys
out, source_sha, bm_d, dmg_d, gw, sc, semver = sys.argv[1:]
doc = {
  "schema_version": 1,
  "source_sha": source_sha,
  "semver": semver,
  "pack_mode": "production",
  "bundle_manifest_digest": bm_d,
  "dmg_sha256": dmg_d,
  "stapled": True,
  "gateway_digest": gw,
  "sidecar_digest": sc,
}
open(out, "w", encoding="utf-8").write(
  json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
)
PY
}

write_proof() {
  local path="$1" kind="$2" cid="$3" sha="$4" result="$5" extra="${6-}"
  [[ -n "$extra" ]] || extra='{}'
  python3 - "$path" "$kind" "$cid" "$sha" "$result" "$extra" <<'PY'
import json, sys, uuid
from datetime import datetime, timezone
path, kind, cid, sha, result, extra_raw = sys.argv[1:]
extra = json.loads(extra_raw)
doc = {
  "schema_version": 1,
  "proof_kind": kind,
  "candidate_id": cid,
  "source_sha": sha,
  "result": result,
  "tool_version": "irin-w5-fake-gh/1",
  "run_id": str(uuid.uuid4()),
  "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
}
for k, v in extra.items():
    doc[k] = v
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY
}

# Promote a production candidate and forge Accepted (not via record-acceptance tty).
promote_accepted() {
  local dmg_body="$1"
  local staging="$TEST_HOME/stage-$$"
  make_staging "$staging" "$dmg_body"
  local cid dest
  cid="$(irin_sha256_file "$staging/candidate.json")"
  dest="$IRIN_CANDIDATE_ROOT/$SEMVER/$SHA/$cid"
  irin_promote_candidate_from_staging "$staging" "$dest" >/dev/null
  chmod -R u+w "$dest/proofs" "$dest/install" 2>/dev/null || true

  local bm_d dmg_d
  bm_d="$(irin_sha256_file "$dest/bundle-manifest.txt")"
  dmg_d="$(python3 -c 'import json; print(json.load(open("'"$dest"'/candidate.json"))["dmg_sha256"])')"

  write_proof "$dest/proofs/verify.json" "verify" "$cid" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
    "dmg_sha256": "'"$dmg_d"'",
    "bundle_manifest_digest": "'"$bm_d"'",
  }))')"

  mkdir -p "$dest/install"
  cp -R "$dest/IRIN.app" "$dest/install/IRIN.app"
  chmod -R u+w "$dest/install" 2>/dev/null || true
  irin_write_bundle_manifest "$dest/install/IRIN.app" "$dest/install/bundle-manifest.txt"
  write_proof "$dest/proofs/install.json" "install" "$cid" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
    "candidate_bundle_manifest_digest": "'"$bm_d"'",
    "installed_bundle_manifest_digest": "'"$bm_d"'",
  }))')"

  local action_id="t2-w5-hermetic"
  write_proof "$dest/proofs/acceptance.json" "acceptance" "$cid" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
    "dmg_sha256": "'"$dmg_d"'",
    "installed_bundle_manifest_digest": "'"$bm_d"'",
    "pending_action_id": "'"$action_id"'",
    "installed_app_path": "'"$dest"'/install/IRIN.app",
  }))')"
  local acc_d
  acc_d="$(irin_sha256_file "$dest/proofs/acceptance.json")"
  write_proof "$dest/proofs/t2.json" "t2" "$cid" "$SHA" "PASS" "$(python3 -c 'import json; print(json.dumps({
    "action_id": "'"$action_id"'",
    "acceptance_digest": "'"$acc_d"'",
    "authorized_effects": ["tag-push", "release-attach", "publish", "version-image-labels"],
    "expiry": "2099-01-01T00:00:00Z",
  }))')"

  printf '%s\n' "$dest"
}

# --- stateful fake transport ------------------------------------------------
STATE="$TEST_HOME/fake-state"
FAKEBIN="$TEST_HOME/fakebin"
mkdir -p "$STATE" "$FAKEBIN" "$STATE/assets"

# Release state: isDraft, assets{name: content_path}, mutations log
python3 - "$STATE/release.json" <<'PY'
import json, sys
json.dump({
  "isDraft": True,
  "id": 4242,
  "html_url": "https://example.test/releases/tag/v9.9.9",
  "assets": {},
}, open(sys.argv[1], "w"))
PY
: >"$STATE/mutations.log"
: >"$STATE/uploads.log"

# Workflow run list JSON (bound to SOURCE_SHA)
python3 -c 'import json; print(json.dumps([
  {"databaseId": 77, "headSha": "'"$SHA"'", "status": "completed",
   "conclusion": "success", "headBranch": "'"$TAG"'", "event": "push",
   "displayTitle": "IRIN Release", "name": "release"},
]))' >"$STATE/gh-runs.json"

cat >"$FAKEBIN/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE="${IRIN_FAKE_GH_STATE:?}"
log() { printf '%s\n' "$*" >>"$STATE/mutations.log"; }

# gh auth token
if [[ "$1" == "auth" && "$2" == "token" ]]; then
  printf 'fake-token\n'
  exit 0
fi

# gh api user --jq .login
if [[ "$1" == "api" && "$2" == "user" ]]; then
  if [[ "${3:-}" == "--jq" ]]; then
    printf 'fake-user\n'
  else
    printf '{"login":"fake-user"}\n'
  fi
  exit 0
fi

# gh run list ...
if [[ "$1" == "run" && "$2" == "list" ]]; then
  cat "$STATE/gh-runs.json"
  exit 0
fi

# gh api repos/.../releases/tags/TAG
if [[ "$1" == "api" && "$2" == repos/*/*/releases/tags/* ]]; then
  python3 - "$STATE/release.json" <<'PY'
import json, sys
rel = json.load(open(sys.argv[1]))
assets = []
for name, path in (rel.get("assets") or {}).items():
    assets.append({
      "name": name,
      "id": abs(hash(name)) % 10_000_000 + 1,
      "browser_download_url": f"https://example.test/download/{name}",
    })
out = {
  "id": rel.get("id", 4242),
  "html_url": rel.get("html_url", "https://example.test/release"),
  "tag_name": "v9.9.9",
  "draft": bool(rel.get("isDraft", True)),
  "assets": assets,
}
print(json.dumps(out))
PY
  exit 0
fi

# gh release view TAG --json ...
if [[ "$1" == "release" && "$2" == "view" ]]; then
  tag="$3"
  shift 3
  fields=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --json) fields="$2"; shift 2 ;;
      --jq) jq_expr="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  python3 - "$STATE/release.json" "${fields:-}" "${jq_expr:-}" <<'PY'
import json, sys
rel = json.load(open(sys.argv[1]))
fields = sys.argv[2] if len(sys.argv) > 2 else ""
jq_expr = sys.argv[3] if len(sys.argv) > 3 else ""
assets = []
for name, path in (rel.get("assets") or {}).items():
    assets.append({"name": name, "id": abs(hash(name)) % 10_000_000 + 1})
doc = {
  "isDraft": bool(rel.get("isDraft", True)),
  "url": rel.get("html_url", "https://example.test/release"),
  "tagName": "v9.9.9",
  "assets": assets,
}
if jq_expr == ".isDraft":
    print("true" if doc["isDraft"] else "false")
elif fields:
    # Subset for --json fields (comma-separated)
    keys = [k.strip() for k in fields.split(",") if k.strip()]
    out = {k: doc[k] for k in keys if k in doc}
    print(json.dumps(out))
else:
    print(json.dumps(doc))
PY
  exit 0
fi

# gh release download TAG -p NAME -O PATH [--clobber]
if [[ "$1" == "release" && "$2" == "download" ]]; then
  tag="$3"
  shift 3
  pattern="" out=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -p) pattern="$2"; shift 2 ;;
      -O) out="$2"; shift 2 ;;
      --clobber) shift ;;
      *) shift ;;
    esac
  done
  path="$(python3 -c 'import json,sys; a=json.load(open(sys.argv[1])).get("assets") or {}; print(a.get(sys.argv[2],""))' "$STATE/release.json" "$pattern")"
  [[ -n "$path" && -f "$path" ]] || { echo "fake-gh: asset not found: $pattern" >&2; exit 1; }
  cp "$path" "$out"
  log "download $pattern -> $out"
  exit 0
fi

# gh release upload TAG FILE  (must never see --clobber)
if [[ "$1" == "release" && "$2" == "upload" ]]; then
  tag="$3"
  file="$4"
  shift 4 || true
  for a in "$@"; do
    if [[ "$a" == "--clobber" ]]; then
      echo "fake-gh: refused --clobber on release upload" >&2
      exit 3
    fi
  done
  name="$(basename "$file")"
  # Refuse clobber of different existing asset at the fake layer too.
  existing="$(python3 -c 'import json,sys; a=json.load(open(sys.argv[1])).get("assets") or {}; print(a.get(sys.argv[2],""))' "$STATE/release.json" "$name")"
  if [[ -n "$existing" ]]; then
    echo "fake-gh: asset already exists (no clobber): $name" >&2
    exit 4
  fi
  dest="$STATE/assets/$name"
  cp "$file" "$dest"
  python3 - "$STATE/release.json" "$name" "$dest" <<'PY'
import json, sys
path, name, dest = sys.argv[1:]
rel = json.load(open(path))
rel.setdefault("assets", {})[name] = dest
json.dump(rel, open(path, "w"))
PY
  printf 'upload %s\n' "$name" >>"$STATE/uploads.log"
  log "upload $name"
  exit 0
fi

# gh release edit TAG --draft=false
if [[ "$1" == "release" && "$2" == "edit" ]]; then
  tag="$3"
  shift 3
  for a in "$@"; do
    if [[ "$a" == "--draft=false" || "$a" == "--draft" && "${2:-}" == "false" ]]; then
      python3 - "$STATE/release.json" <<'PY'
import json, sys
rel = json.load(open(sys.argv[1]))
rel["isDraft"] = False
json.dump(rel, open(sys.argv[1], "w"))
PY
      log "publish draft=false"
      exit 0
    fi
  done
  echo "fake-gh: unhandled release edit: $*" >&2
  exit 2
fi

echo "fake-gh: unexpected $*" >&2
exit 2
EOF
chmod +x "$FAKEBIN/gh"

# Fake docker: imagetools inspect/create for version labels only.
cat >"$FAKEBIN/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE="${IRIN_FAKE_GH_STATE:?}"
LABELS="$STATE/labels.json"
[[ -f "$LABELS" ]] || printf '{}\n' >"$LABELS"

if [[ "$1" == "login" ]]; then
  # Hermetic path skips login; if called, succeed.
  exit 0
fi

if [[ "$1" == "buildx" && "$2" == "imagetools" && "$3" == "inspect" ]]; then
  ref="$4"
  # --format '{{.Manifest.Digest}}'
  digest="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get(sys.argv[2],""))' "$LABELS" "$ref")"
  if [[ -z "$digest" ]]; then
    # Digest-pinned refs resolve to themselves when already sha256-bound.
    case "$ref" in
      *@sha256:*)
        printf '%s\n' "${ref##*@}"
        exit 0
        ;;
      *@*)
        printf '%s\n' "${ref##*@}"
        exit 0
        ;;
    esac
    exit 1
  fi
  printf '%s\n' "$digest"
  exit 0
fi

if [[ "$1" == "buildx" && "$2" == "imagetools" && "$3" == "create" ]]; then
  # --tag IMAGE:TAG DIGEST_REF
  tag_ref=""
  src=""
  shift 3
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --tag) tag_ref="$2"; shift 2 ;;
      *) src="$1"; shift ;;
    esac
  done
  dig="${src##*@}"
  case "$dig" in sha256:*) ;; *) dig="sha256:$dig" ;; esac
  python3 - "$LABELS" "$tag_ref" "$dig" <<'PY'
import json, sys
path, tag, dig = sys.argv[1:]
d = json.load(open(path))
d[tag] = dig
json.dump(d, open(path, "w"))
PY
  printf 'docker-create %s -> %s\n' "$tag_ref" "$dig" >>"$STATE/mutations.log"
  exit 0
fi

echo "fake-docker: unexpected $*" >&2
exit 2
EOF
chmod +x "$FAKEBIN/docker"

# Fake curl: map public download URLs to stored asset bytes.
cat >"$FAKEBIN/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STATE="${IRIN_FAKE_GH_STATE:?}"
out=""
url=""
args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
  a="${args[$i]}"
  case "$a" in
    -o)
      i=$((i + 1))
      out="${args[$i]}"
      ;;
    -H|--proto|--tlsv1.2)
      # consume flag value when present
      i=$((i + 1))
      ;;
    -fsSL|-f|-s|-S|-L) ;;
    https://*|http://*)
      url="$a"
      ;;
  esac
  i=$((i + 1))
done
name="$(basename "${url:-}")"
path="$(python3 -c 'import json,sys; a=json.load(open(sys.argv[1])).get("assets") or {}; print(a.get(sys.argv[2],""))' "$STATE/release.json" "$name")"
[[ -n "$path" && -f "$path" && -n "$out" ]] || {
  echo "fake-curl: cannot serve url=$url out=$out name=$name" >&2
  exit 1
}
cp "$path" "$out"
printf 'curl %s\n' "$url" >>"$STATE/mutations.log"
exit 0
EOF
chmod +x "$FAKEBIN/curl"

export IRIN_FAKE_GH_STATE="$STATE"
export PATH="$FAKEBIN:$PATH"
export IRIN_PUBLISH_HERMETIC=1
export IRIN_PUBLISH_REMOTE_TAG_SHA=""
export IRIN_RELEASE_WORKFLOW_WAIT_ATTEMPTS=3
export IRIN_RELEASE_WORKFLOW_WAIT_SLEEP=0
export IRIN_RELEASE_DRAFT_WAIT_ATTEMPTS=3
export IRIN_RELEASE_DRAFT_WAIT_SLEEP=0
export IRIN_CANDIDATE_STATUS_HERMETIC=1
export IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true
export IRIN_CANDIDATE_STATUS_CI_REQUIRED=true
export GHCR_USERNAME=fake-user
export GHCR_TOKEN=fake-token

run_publish() {
  local cand="$1"
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  IRIN_PUBLISH_HERMETIC=1 \
  IRIN_PUBLISH_REMOTE_TAG_SHA="${IRIN_PUBLISH_REMOTE_TAG_SHA-}" \
  PATH="$FAKEBIN:$PATH" \
  "$TX" --publish --tag "$TAG" --candidate "$cand" --t2-packet "$cand/proofs/t2.json"
}

# --- static: upload path never uses --clobber ------------------------------
grep -n 'gh release upload' "$TX" | grep -q -- '--clobber' \
  && fail "release-transaction must not pass --clobber to gh release upload" || true
# Positive: upload line exists without clobber.
grep -q 'gh release upload' "$TX" || fail "missing gh release upload"
pass "source: gh release upload has no --clobber"

# Unique DMG body per case → distinct candidate-id (no shared-store residue).
dmg_body() { printf 'dmg-body-%s-%s' "$1" "$$"; }

# --- [1] draft alone (no workflow success) refuses -------------------------
# Empty run list → wait times out before draft attach.
printf '[]\n' >"$STATE/gh-runs.json"
DEST1="$(promote_accepted "$(dmg_body draft-only)")"
DMG1="$(find "$DEST1" -maxdepth 1 -type f -name '*.dmg' | head -1)"
ASSET_NAME="$(basename "$DMG1")"
# Seed a draft so "draft alone" is present.
python3 - "$STATE/release.json" <<'PY'
import json, sys
json.dump({
  "isDraft": True,
  "id": 4242,
  "html_url": "https://example.test/releases/tag/v9.9.9",
  "assets": {},
}, open(sys.argv[1], "w"))
PY
set +e
out="$(run_publish "$DEST1" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "draft alone without workflow success must refuse: $out"
[[ "$out" == *"timed out"* || "$out" == *"no release.yml"* || "$out" == *"workflow"* ]] \
  || fail "expected workflow refuse for draft-only: $out"
[[ ! -f "$DEST1/proofs/publication.json" ]] \
  || fail "draft-only path must not write publication.json"
pass "draft existence alone without workflow success refuses"

# Restore successful workflow runs for remaining cases.
python3 -c 'import json; print(json.dumps([
  {"databaseId": 77, "headSha": "'"$SHA"'", "status": "completed",
   "conclusion": "success", "headBranch": "'"$TAG"'", "event": "push",
   "displayTitle": "IRIN Release", "name": "release"},
]))' >"$STATE/gh-runs.json"

# --- [2] different existing asset refuses (no clobber) ---------------------
: >"$STATE/mutations.log"
: >"$STATE/uploads.log"
DEST_DIFF="$(promote_accepted "$(dmg_body different-asset)")"
DMG_DIFF="$(find "$DEST_DIFF" -maxdepth 1 -type f -name '*.dmg' | head -1)"
ASSET_DIFF="$(basename "$DMG_DIFF")"
WRONG="$STATE/assets/wrong.dmg"
printf 'WRONG-ASSET-BYTES' >"$WRONG"
python3 - "$STATE/release.json" "$ASSET_DIFF" "$WRONG" <<'PY'
import json, sys
path, name, wrong = sys.argv[1:]
json.dump({
  "isDraft": True,
  "id": 4242,
  "html_url": "https://example.test/releases/tag/v9.9.9",
  "assets": {name: wrong},
}, open(path, "w"))
PY
set +e
out="$(run_publish "$DEST_DIFF" 2>&1)"
ec=$?
set -e
[[ $ec -ne 0 ]] || fail "different draft asset must refuse: $out"
[[ "$out" == *"refusing"* || "$out" == *"no clobber"* || "$out" == *"hash"* ]] \
  || fail "expected hash/clobber refuse: $out"
# No upload mutation, asset still wrong bytes.
[[ ! -s "$STATE/uploads.log" ]] || fail "must not upload when existing asset differs"
got="$(cat "$WRONG")"
[[ "$got" == "WRONG-ASSET-BYTES" ]] || fail "existing asset bytes must be unchanged"
pass "different existing draft asset refuses (no clobber)"

# --- [3] equal existing asset is idempotent; publish completes -------------
: >"$STATE/mutations.log"
: >"$STATE/uploads.log"
printf '{}\n' >"$STATE/labels.json"
DEST2="$(promote_accepted "$(dmg_body equal-asset)")"
DMG2="$(find "$DEST2" -maxdepth 1 -type f -name '*.dmg' | head -1)"
[[ -n "$DMG2" && -f "$DMG2" ]] || fail "DEST2 DMG missing"
EQUAL="$STATE/assets/equal.dmg"
rm -f "$EQUAL"
# Copy via python so frozen (0555) source bytes still land as a writable fake asset.
python3 -c 'import shutil,sys; shutil.copyfile(sys.argv[1], sys.argv[2])' "$DMG2" "$EQUAL"
chmod u+w "$EQUAL" 2>/dev/null || true
python3 - "$STATE/release.json" "$(basename "$DMG2")" "$EQUAL" <<'PY'
import json, sys
path, name, equal = sys.argv[1:]
json.dump({
  "isDraft": True,
  "id": 4242,
  "html_url": "https://example.test/releases/tag/v9.9.9",
  "assets": {name: equal},
}, open(path, "w"))
PY

out="$(run_publish "$DEST2" 2>&1)" || fail "equal asset publish should succeed: $out"
[[ "$out" == *"idempotent skip upload"* || "$out" == *"hash matches"* ]] \
  || fail "expected idempotent skip: $out"
[[ ! -s "$STATE/uploads.log" ]] || fail "equal asset must not re-upload"
[[ -f "$DEST2/proofs/publication.json" ]] || fail "publication.json missing after public path"
# Draft→public transition must have occurred.
is_draft="$(python3 -c 'import json; print(json.load(open("'"$STATE"'/release.json"))["isDraft"])')"
[[ "$is_draft" == "False" ]] || fail "release must be public after publish (got isDraft=$is_draft)"
tier="$(
  IRIN_CANDIDATE_STATUS_HERMETIC=1 \
  IRIN_CANDIDATE_STATUS_SOURCE_ON_MAIN=true \
  IRIN_CANDIDATE_STATUS_CI_REQUIRED=true \
  "$STATUS" --candidate "$DEST2" --json \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tier") or "")'
)"
[[ "$tier" == "Published" ]] || fail "expected Published tier, got '$tier'"
pass "equal draft asset is idempotent; public re-download writes publication proof"

# Snapshot mutation log after first successful publish.
cp "$STATE/mutations.log" "$STATE/mutations.after-first.log"
UPLOAD_COUNT_1="$(wc -l <"$STATE/uploads.log" | tr -d ' ')"

# --- [4] public retry: no mutation, proof re-validates ---------------------
# publication.json already present; release public; labels already set.
set +e
out2="$(run_publish "$DEST2" 2>&1)"
ec2=$?
set -e
[[ $ec2 -eq 0 ]] || fail "public retry should succeed: $out2"
[[ "$out2" == *"validation-only"* || "$out2" == *"already public"* || "$out2" == *"publication proof already"* ]] \
  || fail "expected validation-only public retry: $out2"
# No new uploads.
UPLOAD_COUNT_2="$(wc -l <"$STATE/uploads.log" | tr -d ' ')"
[[ "$UPLOAD_COUNT_2" == "$UPLOAD_COUNT_1" ]] || fail "public retry must not upload"
first_mut="$(grep -cE 'upload |publish draft' "$STATE/mutations.after-first.log" || true)"
now_mut="$(grep -cE 'upload |publish draft' "$STATE/mutations.log" || true)"
[[ "$now_mut" -eq "$first_mut" ]] || fail "public retry mutated release (upload/publish grew)"
pass "public retry finishes proof without mutation"

# --- [5] fresh upload path + draft re-download is not Published mid-way ----
# Reset to empty draft, no assets; new candidate.
: >"$STATE/mutations.log"
: >"$STATE/uploads.log"
python3 - "$STATE/release.json" <<'PY'
import json, sys
json.dump({
  "isDraft": True,
  "id": 5151,
  "html_url": "https://example.test/releases/tag/v9.9.9",
  "assets": {},
}, open(sys.argv[1], "w"))
PY
printf '{}\n' >"$STATE/labels.json"
DEST3="$(promote_accepted "$(dmg_body fresh-upload)")"
# Prove ordering in source: draft integrity check → publish → public re-download → proof.
python3 - "$TX" <<'PY' || fail "publication proof must follow public re-download"
import sys
text = open(sys.argv[1]).read()
pub = text.split("do_publish()")[1]
assert "authenticated draft re-download" in pub
assert "not Published" in pub or "upload integrity only" in pub
i_draft = pub.find("authenticated draft re-download")
i_edit = pub.find("gh release edit")
i_public = pub.find("unauthenticated public re-download")
i_write = pub.find("publication.json")
assert 0 < i_draft < i_edit < i_public < i_write, (
    f"order wrong: draft={i_draft} edit={i_edit} public={i_public} write={i_write}"
)
print("draft-before-public-before-proof order ok")
PY
out3="$(run_publish "$DEST3" 2>&1)" || fail "fresh upload publish should succeed: $out3"
[[ -s "$STATE/uploads.log" ]] || fail "fresh path must upload once"
[[ "$(wc -l <"$STATE/uploads.log" | tr -d ' ')" == "1" ]] || fail "expected exactly one upload"
[[ -f "$DEST3/proofs/publication.json" ]] || fail "publication.json missing after fresh publish"
# Public proof must claim unauthenticated redownload.
python3 - "$DEST3/proofs/publication.json" <<'PY' || fail "publication envelope incomplete"
import json, sys
d = json.load(open(sys.argv[1]))
assert d["proof_kind"] == "publication"
assert d["result"] == "PASS"
assert d.get("public_state") in ("published", "public")
assert d.get("redownload_unauthenticated") is True
assert d.get("asset_sha256") or d.get("dmg_sha256")
print("publication envelope ok")
PY
pass "fresh upload + unauthenticated public re-download writes publication proof"

printf '\nAll fake-gh publication contracts passed.\n'
