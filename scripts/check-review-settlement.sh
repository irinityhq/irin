#!/usr/bin/env bash
# Fail-closed pre-queue review settlement evaluator for IRIN.
#
# Settlement (this check) means: no pending review requests, every
# non-dismissed latest review is bound to the current headRefOid (a new commit
# invalidates prior settlement), and no CHANGES_REQUESTED on head.
#
# Review *threads* are intentionally out of scope here. GitHub emits no
# supported Actions event when a conversation is resolved, so a custom required
# check that failed on unresolved threads would stick red until a manual rerun.
# Required conversation-resolution branch protection / ruleset owns thread
# blocking; this check does not re-implement it.
#
# GraphQL connections are first:100. pageInfo.hasNextPage is inspected and any
# truncated connection fails closed (exit 2) rather than reporting SETTLED.
#
# Modes:
#   --snapshot PATH   evaluate a normalized JSON snapshot (contract tests)
#   --owner O --repo R --pr N   fetch live state via gh graphql
#   --self-test       run embedded fixture contracts and exit
#
# Exit codes: 0 settled, 1 not settled, 2 usage / transport / schema / truncated.
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

# Evaluate a normalized snapshot file. Exit 0 settled, 1 not settled, 2 schema.
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

# Fail closed on truncated GraphQL connections (pageInfo.hasNextPage).
truncated = data.get("truncatedConnections")
if truncated is not None:
    if not isinstance(truncated, list):
        print("schema: truncatedConnections must be a list", file=sys.stderr)
        sys.exit(2)
    if truncated:
        names = ", ".join(str(x) for x in truncated)
        print(
            f"schema: GraphQL connection truncated (hasNextPage): {names}",
            file=sys.stderr,
        )
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

# reviewThreads are intentionally ignored: conversation-resolution owns them.

if reasons:
    print("review-settlement: NOT SETTLED")
    for r in reasons:
        print(f"  - {r}")
    sys.exit(1)

print(f"review-settlement: SETTLED on {head}")
sys.exit(0)
PY
}

# Normalize a GraphQL pullRequest object (or full response) to a snapshot file.
# Fails closed (exit 2) when pageInfo.hasNextPage is true for a watched connection.
# Writes snapshot JSON to $2. Used by live fetch and --self-test fixtures.
normalize_graphql_to_snapshot() {
  local graphql_path="$1" snapshot_path="$2"
  python3 - "$graphql_path" "$snapshot_path" <<'PY'
import json
import sys
from pathlib import Path

src = Path(sys.argv[1])
dst = Path(sys.argv[2])
try:
    raw = json.loads(src.read_text(encoding="utf-8"))
except Exception as exc:  # noqa: BLE001
    print(f"schema: cannot parse graphql payload: {exc}", file=sys.stderr)
    sys.exit(2)

if not isinstance(raw, dict):
    print("schema: graphql payload must be an object", file=sys.stderr)
    sys.exit(2)

if raw.get("errors"):
    print("graphql errors: " + json.dumps(raw["errors"]), file=sys.stderr)
    sys.exit(2)

# Accept either a full GraphQL response or a bare pullRequest object.
if "data" in raw:
    pr = (raw.get("data") or {}).get("repository", {}).get("pullRequest")
else:
    pr = raw.get("pullRequest", raw)

if not pr or not isinstance(pr, dict):
    print("graphql: pullRequest missing", file=sys.stderr)
    sys.exit(2)

WATCHED = ("reviewRequests", "latestReviews")
truncated = []
for name in WATCHED:
    conn = pr.get(name)
    if conn is None:
        # Missing connection is a schema failure (do not invent empty).
        print(f"schema: GraphQL connection missing: {name}", file=sys.stderr)
        sys.exit(2)
    if not isinstance(conn, dict):
        print(f"schema: GraphQL connection {name} must be an object", file=sys.stderr)
        sys.exit(2)
    page = conn.get("pageInfo")
    if not isinstance(page, dict) or "hasNextPage" not in page:
        print(
            f"schema: GraphQL connection {name} missing pageInfo.hasNextPage",
            file=sys.stderr,
        )
        sys.exit(2)
    if page.get("hasNextPage") is True:
        truncated.append(name)

if truncated:
    # Fail closed: never report SETTLED from a partial page.
    print(
        "schema: GraphQL connection truncated (hasNextPage): "
        + ", ".join(truncated),
        file=sys.stderr,
    )
    sys.exit(2)

requests = []
for node in (pr.get("reviewRequests") or {}).get("nodes") or []:
    rr = (node or {}).get("requestedReviewer") or {}
    login = (
        rr.get("login")
        or rr.get("combinedSlug")
        or rr.get("slug")
        or rr.get("name")
    )
    if login:
        requests.append({"login": login})
    else:
        requests.append({"login": rr.get("__typename") or "unknown"})

reviews = []
for node in (pr.get("latestReviews") or {}).get("nodes") or []:
    author = ((node or {}).get("author") or {}).get("login") or "unknown"
    commit = ((node or {}).get("commit") or {}).get("oid") or ""
    reviews.append(
        {
            "author": author,
            "state": (node or {}).get("state") or "",
            "commitOid": commit,
        }
    )

out = {
    "headRefOid": pr.get("headRefOid") or "",
    "isDraft": bool(pr.get("isDraft")),
    "reviewRequests": requests,
    "latestReviews": reviews,
    "truncatedConnections": [],
}
dst.write_text(json.dumps(out) + "\n", encoding="utf-8")
sys.exit(0)
PY
}

fetch_and_evaluate() {
  local owner="$1" repo="$2" pr="$3"
  command -v gh >/dev/null || die "gh is required for live evaluation"
  command -v python3 >/dev/null || die "python3 is required"

  local raw_tmp snap_tmp
  # macOS mktemp requires trailing X's (no suffix after the template).
  raw_tmp="$(mktemp "${TMPDIR:-/tmp}/irin-review-settlement-raw.XXXXXX")"
  snap_tmp="$(mktemp "${TMPDIR:-/tmp}/irin-review-settlement-snap.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -f '$raw_tmp' '$snap_tmp'" RETURN

  # GraphQL: head SHA, pending requests, latest reviews. pageInfo is mandatory
  # so a first:100 cap cannot fail open. Threads are not queried: conversation
  # resolution owns them (no reliable Actions event on resolve).
  if ! gh api graphql \
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
        pageInfo { hasNextPage }
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
        pageInfo { hasNextPage }
        nodes {
          author { login }
          state
          commit { oid }
        }
      }
    }
  }
}' >"$raw_tmp"; then
    die "gh graphql failed for ${owner}/${repo}#${pr}"
  fi

  normalize_graphql_to_snapshot "$raw_tmp" "$snap_tmp"
  evaluate_snapshot "$snap_tmp"
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

  expect_normalize() {
    local name="$1" want="$2"  # want: ok|error
    local rc=0
    set +e
    out="$(normalize_graphql_to_snapshot "$tmp/$name.graphql.json" "$tmp/$name.snap.json" 2>&1)"
    rc=$?
    set -e
    case "$want" in
      ok)
        if (( rc == 0 )); then
          printf 'PASS: %s (normalize ok)\n' "$name"
        else
          printf 'FAIL: %s expected normalize ok rc=0 got rc=%s\n%s\n' "$name" "$rc" "$out" >&2
          failures=$((failures + 1))
        fi
        ;;
      error)
        if (( rc == 2 )); then
          printf 'PASS: %s (normalize error)\n' "$name"
        else
          printf 'FAIL: %s expected normalize error rc=2 got rc=%s\n%s\n' "$name" "$rc" "$out" >&2
          failures=$((failures + 1))
        fi
        ;;
      *)
        die "bad expect_normalize want=$want"
        ;;
    esac
  }

  local head="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local old="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

  # PR #70 pre-merge: Copilot requested, no review yet → fail closed.
  write_case pr70_pending_request "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[{"login":"copilot-pull-request-reviewer"}],"latestReviews":[],"truncatedConnections":[]}
EOF
)"
  expect pr70_pending_request unsettled

  # Threads are ignored by this check (conversation-resolution owns them).
  # A snapshot that still carries thread-like noise must settle if request/review
  # state is clean — proves we do not re-implement thread blocking here.
  write_case threads_ignored_settle "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$head"}],"reviewThreads":[{"isResolved":false,"isOutdated":false}],"truncatedConnections":[]}
EOF
)"
  expect threads_ignored_settle settled

  # New commit invalidates prior review (not on headRefOid).
  write_case stale_review_after_push "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$old"}],"truncatedConnections":[]}
EOF
)"
  expect stale_review_after_push unsettled

  write_case stale_approval "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"reviewer","state":"APPROVED","commitOid":"$old"}],"truncatedConnections":[]}
EOF
)"
  expect stale_approval unsettled

  write_case changes_requested_on_head "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"reviewer","state":"CHANGES_REQUESTED","commitOid":"$head"}],"truncatedConnections":[]}
EOF
)"
  expect changes_requested_on_head unsettled

  # Clean solo PR: no requests, no reviews.
  write_case clean_solo "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[],"truncatedConnections":[]}
EOF
)"
  expect clean_solo settled

  write_case settled_on_head "$(cat <<EOF
{"headRefOid":"$head","isDraft":false,"reviewRequests":[],"latestReviews":[{"author":"copilot-pull-request-reviewer","state":"COMMENTED","commitOid":"$head"}],"truncatedConnections":[]}
EOF
)"
  expect settled_on_head settled

  write_case draft_ok "$(cat <<EOF
{"headRefOid":"$head","isDraft":true,"reviewRequests":[{"login":"copilot-pull-request-reviewer"}],"latestReviews":[],"truncatedConnections":[]}
EOF
)"
  expect draft_ok settled

  # Truncated GraphQL page → fail closed (exit 2), never SETTLED.
  write_case truncated_requests error
  printf '%s\n' "{\"headRefOid\":\"$head\",\"isDraft\":false,\"reviewRequests\":[],\"latestReviews\":[],\"truncatedConnections\":[\"reviewRequests\"]}" \
    >"$tmp/truncated_requests.json"
  expect truncated_requests error

  write_case truncated_reviews error
  printf '%s\n' "{\"headRefOid\":\"$head\",\"isDraft\":false,\"reviewRequests\":[],\"latestReviews\":[],\"truncatedConnections\":[\"latestReviews\"]}" \
    >"$tmp/truncated_reviews.json"
  expect truncated_reviews error

  write_case missing_head error
  printf '%s\n' '{"reviewRequests":[],"latestReviews":[],"truncatedConnections":[]}' >"$tmp/missing_head.json"
  expect missing_head error

  # Normalize path: hasNextPage true fails closed; false normalizes.
  write_case gql_truncated_requests ''
  cat >"$tmp/gql_truncated_requests.graphql.json" <<EOF
{"data":{"repository":{"pullRequest":{
  "isDraft":false,
  "headRefOid":"$head",
  "reviewRequests":{"pageInfo":{"hasNextPage":true},"nodes":[]},
  "latestReviews":{"pageInfo":{"hasNextPage":false},"nodes":[]}
}}}}
EOF
  expect_normalize gql_truncated_requests error

  write_case gql_truncated_reviews ''
  cat >"$tmp/gql_truncated_reviews.graphql.json" <<EOF
{"data":{"repository":{"pullRequest":{
  "isDraft":false,
  "headRefOid":"$head",
  "reviewRequests":{"pageInfo":{"hasNextPage":false},"nodes":[]},
  "latestReviews":{"pageInfo":{"hasNextPage":true},"nodes":[]}
}}}}
EOF
  expect_normalize gql_truncated_reviews error

  write_case gql_missing_pageinfo ''
  cat >"$tmp/gql_missing_pageinfo.graphql.json" <<EOF
{"data":{"repository":{"pullRequest":{
  "isDraft":false,
  "headRefOid":"$head",
  "reviewRequests":{"nodes":[]},
  "latestReviews":{"pageInfo":{"hasNextPage":false},"nodes":[]}
}}}}
EOF
  expect_normalize gql_missing_pageinfo error

  write_case gql_ok ''
  cat >"$tmp/gql_ok.graphql.json" <<EOF
{"data":{"repository":{"pullRequest":{
  "isDraft":false,
  "headRefOid":"$head",
  "reviewRequests":{"pageInfo":{"hasNextPage":false},"nodes":[{"requestedReviewer":{"login":"copilot-pull-request-reviewer"}}]},
  "latestReviews":{"pageInfo":{"hasNextPage":false},"nodes":[]}
}}}}
EOF
  expect_normalize gql_ok ok
  if [[ -f "$tmp/gql_ok.snap.json" ]]; then
    set +e
    out="$(evaluate_snapshot "$tmp/gql_ok.snap.json" 2>&1)"
    rc=$?
    set -e
    if (( rc == 1 )); then
      printf 'PASS: gql_ok snapshot evaluates unsettled (pending request)\n'
    else
      printf 'FAIL: gql_ok snapshot expected unsettled rc=1 got rc=%s\n%s\n' "$rc" "$out" >&2
      failures=$((failures + 1))
    fi
  fi

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
