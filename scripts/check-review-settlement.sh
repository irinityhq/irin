#!/usr/bin/env bash
# Fail-closed pre-queue review settlement evaluator for IRIN.
#
# Settlement means: no pending review requests, every non-dismissed latest
# review is bound to the current headRefOid (a new commit invalidates prior
# settlement), no CHANGES_REQUESTED on head, and no actionable unresolved
# review threads. Draft PRs are treated as not-ready (pass with note) so the
# check does not block drafting; ready_for_review re-runs evaluation.
#
# Modes:
#   --snapshot PATH   evaluate a normalized JSON snapshot (contract tests)
#   --owner O --repo R --pr N   fetch live state via gh graphql
#   --self-test       run embedded fixture contracts and exit
#
# Exit codes: 0 settled, 1 not settled, 2 usage / transport / schema error.
set -euo pipefail

usage() {
  cat <<'EOF' >&2
usage:
  scripts/check-review-settlement.sh --snapshot PATH
  scripts/check-review-settlement.sh --owner OWNER --repo REPO --pr N
  scripts/check-review-settlement.sh --self-test
EOF
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 2
}

# Evaluate a normalized snapshot on stdin or as $1 file contents via python.
# Prints one reason per line on failure; exit 0/1.
evaluate_snapshot() {
  local snapshot_path="$1"
  python3 - "$snapshot_path" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception as exc:  # noqa: BLE001 — surface schema/transport errors
    print(f"schema: cannot parse snapshot: {exc}", file=sys.stderr)
    sys.exit(2)

if not isinstance(data, dict):
    print("schema: snapshot must be a JSON object", file=sys.stderr)
    sys.exit(2)

head = data.get("headRefOid")
if not isinstance(head, str) or not head.strip():
    print("schema: headRefOid must be a non-empty string", file=sys.stderr)
    sys.exit(2)
head = head.strip()

if data.get("isDraft") is True:
    print("review-settlement: draft PR — settlement not required yet")
    sys.exit(0)

reasons = []

requests = data.get("reviewRequests")
if requests is None:
    print("schema: reviewRequests required", file=sys.stderr)
    sys.exit(2)
if not isinstance(requests, list):
    print("schema: reviewRequests must be a list", file=sys.stderr)
    sys.exit(2)
for item in requests:
    if isinstance(item, str) and item.strip():
        login = item.strip()
    elif isinstance(item, dict):
        login = (
            item.get("login")
            or item.get("slug")
            or item.get("name")
            or "unknown"
        )
        login = str(login)
    else:
        login = "unknown"
    reasons.append(f"pending_review_request:{login}")

reviews = data.get("latestReviews")
if reviews is None:
    print("schema: latestReviews required", file=sys.stderr)
    sys.exit(2)
if not isinstance(reviews, list):
    print("schema: latestReviews must be a list", file=sys.stderr)
    sys.exit(2)

for rev in reviews:
    if not isinstance(rev, dict):
        reasons.append("schema: latestReviews entry must be object")
        continue
    author = str(rev.get("author") or rev.get("login") or "unknown")
    state = str(rev.get("state") or "").upper()
    commit = rev.get("commitOid") or rev.get("commit") or ""
    if isinstance(commit, dict):
        commit = commit.get("oid") or ""
    commit = str(commit).strip()

    if state in {"", "DISMISSED"}:
        # Dismissed reviews do not settle and do not block.
        continue
    if state == "PENDING":
        reasons.append(f"pending_review:{author}")
        continue
    if not commit:
        reasons.append(f"review_missing_commit:{author}:{state or 'UNKNOWN'}")
        continue
    if commit != head:
        reasons.append(f"review_not_on_head:{author}:{state}:{commit}")
        continue
    if state == "CHANGES_REQUESTED":
        reasons.append(f"changes_requested_on_head:{author}")

threads = data.get("reviewThreads")
if threads is None:
    print("schema: reviewThreads required", file=sys.stderr)
    sys.exit(2)
if not isinstance(threads, list):
    print("schema: reviewThreads must be a list", file=sys.stderr)
    sys.exit(2)

actionable = 0
for thr in threads:
    if not isinstance(thr, dict):
        reasons.append("schema: reviewThreads entry must be object")
        continue
    resolved = bool(thr.get("isResolved"))
    outdated = bool(thr.get("isOutdated"))
    # Actionable = still open on the current discussion surface.
    # Outdated threads are not actionable for settlement (code moved on).
    if not resolved and not outdated:
        actionable += 1
if actionable:
    reasons.append(f"unresolved_actionable_threads:{actionable}")

if reasons:
    print("review-settlement: NOT SETTLED")
    for r in reasons:
        print(f"  - {r}")
    sys.exit(1)

print(f"review-settlement: SETTLED on {head}")
sys.exit(0)
PY
}

fetch_and_evaluate() {
  local owner="$1" repo="$2" pr="$3"
  command -v gh >/dev/null || die "gh is required for live evaluation"
  command -v python3 >/dev/null || die "python3 is required"

  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/irin-review-settlement.XXXXXX.json")"
  # shellcheck disable=SC2064
  trap "rm -f '$tmp'" RETURN

  # GraphQL: head SHA, pending requests, latest reviews per author, threads.
  local raw
  if ! raw="$(
    gh api graphql \
      -f owner="$owner" \
      -f name="$repo" \
      -F number="$pr" \
      -f query='
query($owner:String!,$name:String!,$number:Int!) {
  repository(owner:$owner, name:$name) {
    pullRequest(number:$number) {
      isDraft
      headRefOid
      reviewRequests(first:100) {
        nodes {
          requestedReviewer {
            __typename
            ... on User { login }
            ... on Bot { login }
            ... on Team { combinedSlug slug name }
          }
        }
      }
      latestReviews(first:100) {
        nodes {
          author { login }
          state
          commit { oid }
        }
      }
      reviewThreads(first:100) {
        nodes {
          isResolved
          isOutdated
        }
      }
    }
  }
}'
  )"; then
    die "gh graphql failed for ${owner}/${repo}#${pr}"
  fi

  python3 -c '
import json, sys
raw = json.loads(sys.stdin.read())
if raw.get("errors"):
    print("graphql errors: " + json.dumps(raw["errors"]), file=sys.stderr)
    sys.exit(2)
pr = (raw.get("data") or {}).get("repository", {}).get("pullRequest")
if not pr:
    print("graphql: pullRequest missing", file=sys.stderr)
    sys.exit(2)
requests = []
for node in (pr.get("reviewRequests") or {}).get("nodes") or []:
    rr = (node or {}).get("requestedReviewer") or {}
    login = rr.get("login") or rr.get("combinedSlug") or rr.get("slug") or rr.get("name")
    if login:
        requests.append({"login": login})
    else:
        requests.append({"login": rr.get("__typename") or "unknown"})
reviews = []
for node in (pr.get("latestReviews") or {}).get("nodes") or []:
    author = ((node or {}).get("author") or {}).get("login") or "unknown"
    commit = ((node or {}).get("commit") or {}).get("oid") or ""
    reviews.append({
        "author": author,
        "state": (node or {}).get("state") or "",
        "commitOid": commit,
    })
threads = []
for node in (pr.get("reviewThreads") or {}).get("nodes") or []:
    threads.append({
        "isResolved": bool((node or {}).get("isResolved")),
        "isOutdated": bool((node or {}).get("isOutdated")),
    })
out = {
    "headRefOid": pr.get("headRefOid") or "",
    "isDraft": bool(pr.get("isDraft")),
    "reviewRequests": requests,
    "latestReviews": reviews,
    "reviewThreads": threads,
}
json.dump(out, sys.stdout)
' <<<"$raw" >"$tmp"

  evaluate_snapshot "$tmp"
}

run_self_test() {
  local tmp failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-review-settlement-self.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT

  write_case() {
    local name="$1" json="$2"
    printf '%s\n' "$json" >"$tmp/$name.json"
  }

  expect() {
    local name="$1" want="$2"  # want: settled|unsettled|error
    local rc=0
    set +e
    out="$(evaluate_snapshot "$tmp/$name.json" 2>&1)"
    rc=$?
    set -e
    case "$want" in
      settled)
        if (( rc == 0 )); then
          printf 'PASS: %s (settled)\n' "$name"
        else
          printf 'FAIL: %s expected settled rc=0 got rc=%s\n%s\n' "$name" "$rc" "$out" >&2
          failures=$((failures + 1))
        fi
        ;;
      unsettled)
        if (( rc == 1 )); then
          printf 'PASS: %s (unsettled)\n' "$name"
        else
          printf 'FAIL: %s expected unsettled rc=1 got rc=%s\n%s\n' "$name" "$rc" "$out" >&2
          failures=$((failures + 1))
        fi
        ;;
      error)
        if (( rc == 2 )); then
          printf 'PASS: %s (error)\n' "$name"
        else
          printf 'FAIL: %s expected error rc=2 got rc=%s\n%s\n' "$name" "$rc" "$out" >&2
          failures=$((failures + 1))
        fi
        ;;
      *)
        die "bad expect want=$want"
        ;;
    esac
  }

  local head="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local old="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

  # PR #70 pre-merge: Copilot requested, no review yet → fail closed.
  write_case pr70_pending_request "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[{"login":"copilot-pull-request-reviewer"}],"latestReviews":[],"reviewThreads":[]}
EOF
)"
  expect pr70_pending_request unsettled

  # Review on head but actionable threads remain → fail.
  write_case unresolved_threads "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$head"}],"reviewThreads":[{"isResolved":false,"isOutdated":false},{"isResolved":false,"isOutdated":false}]}
EOF
)"
  expect unresolved_threads unsettled

  # New commit invalidates prior review (not on headRefOid).
  write_case stale_review_after_push "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$old"}],"reviewThreads":[]}
EOF
)"
  expect stale_review_after_push unsettled

  write_case stale_approval "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"reviewer","state":"APPROVED","commitOid":"$old"}],"reviewThreads":[]}
EOF
)"
  expect stale_approval unsettled

  write_case changes_requested_on_head "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"reviewer","state":"CHANGES_REQUESTED","commitOid":"$head"}],"reviewThreads":[]}
EOF
)"
  expect changes_requested_on_head unsettled

  # Outdated unresolved threads are not actionable.
  write_case outdated_threads_ok "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$head"}],"reviewThreads":[{"isResolved":false,"isOutdated":true}]}
EOF
)"
  expect outdated_threads_ok settled

  # Clean solo PR: no requests, no reviews, no threads.
  write_case clean_solo "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[],"reviewThreads":[]}
EOF
)"
  expect clean_solo settled

  # Settled after review on exact head with threads resolved.
  write_case settled_on_head "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$head"}],"reviewThreads":[{"isResolved":true,"isOutdated":false}]}
EOF
)"
  expect settled_on_head settled

  write_case draft_ok "$(cat <<EOF
{"headRefOid":"$head","isDraft":true,"reviewRequests":[{"login":"copilot-pull-request-reviewer"}],"latestReviews":[],"reviewThreads":[]}
EOF
)"
  expect draft_ok settled

  write_case missing_head error
  printf '%s\n' '{"reviewRequests":[],"latestReviews":[],"reviewThreads":[]}' >"$tmp/missing_head.json"
  expect missing_head error

  if (( failures > 0 )); then
    printf 'check-review-settlement self-test: FAILED (%d)\n' "$failures" >&2
    exit 1
  fi
  printf 'check-review-settlement self-test: OK\n'
}

# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------
mode=""
snapshot=""
owner=""
repo=""
pr=""

while (($#)); do
  case "$1" in
    --snapshot)
      mode=snapshot
      snapshot="${2:-}"
      shift 2 || die "--snapshot requires PATH"
      ;;
    --owner)
      owner="${2:-}"
      shift 2 || die "--owner requires value"
      ;;
    --repo)
      repo="${2:-}"
      shift 2 || die "--repo requires value"
      ;;
    --pr)
      pr="${2:-}"
      shift 2 || die "--pr requires value"
      ;;
    --self-test)
      mode=self-test
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      die "unknown argument: $1"
      ;;
  esac
done

case "${mode:-}" in
  self-test)
    run_self_test
    ;;
  snapshot)
    [[ -n "$snapshot" ]] || die "--snapshot requires PATH"
    [[ -f "$snapshot" ]] || die "snapshot not found: $snapshot"
    evaluate_snapshot "$snapshot"
    ;;
  "")
    if [[ -n "$owner" && -n "$repo" && -n "$pr" ]]; then
      [[ "$pr" =~ ^[0-9]+$ ]] || die "--pr must be an integer"
      fetch_and_evaluate "$owner" "$repo" "$pr"
    else
      usage
      die "specify --snapshot, --self-test, or --owner/--repo/--pr"
    fi
    ;;
  *)
    die "internal: bad mode $mode"
    ;;
esac
