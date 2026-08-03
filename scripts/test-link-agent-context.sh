#!/usr/bin/env bash
# Hermetic contract tests for scripts/link-agent-context.sh.
# Disposable git repo + worktree; no network; no IRIN private doctrine required.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$ROOT/scripts/link-agent-context.sh"
[[ -x "$HELPER" ]] || { printf 'FAIL: helper missing or not executable: %s\n' "$HELPER" >&2; exit 1; }

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-link-agent-context.XXXXXX")"
cleanup() {
  if [[ -n "${REPO:-}" && -d "${REPO:-}" ]]; then
    git -C "$REPO" worktree list --porcelain 2>/dev/null | while IFS= read -r line; do
      case "$line" in
        worktree\ *)
          wt="${line#worktree }"
          [[ "$wt" == "$REPO" ]] && continue
          git -C "$REPO" worktree remove --force "$wt" 2>/dev/null || true
          ;;
      esac
    done
  fi
  if [[ -d "$TEST_HOME" ]]; then
    chmod -R u+w "$TEST_HOME" 2>/dev/null || true
    rm -rf "$TEST_HOME"
  fi
}
trap cleanup EXIT

REPO="$TEST_HOME/repo"
WT="$TEST_HOME/wt"
mkdir -p "$REPO/scripts"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.email "test@example.invalid"
git -C "$REPO" config user.name "link-agent-context test"
printf 'tracked\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" commit -q -m "init"

# Shared exclude so doctrine names stay private in every worktree.
mkdir -p "$REPO/.git/info"
cat >>"$REPO/.git/info/exclude" <<'EOF'
AGENTS.md
CLAUDE.md
RTK.md
.projectmem/
EOF

# Canonical doctrine + initialized ledger (real files, not symlinks).
printf 'agents doctrine\n' >"$REPO/AGENTS.md"
printf 'claude doctrine\n' >"$REPO/CLAUDE.md"
printf 'rtk doctrine\n' >"$REPO/RTK.md"
mkdir -p "$REPO/.projectmem"
printf 'summary\n' >"$REPO/.projectmem/summary.md"

cp "$HELPER" "$REPO/scripts/link-agent-context.sh"
chmod +x "$REPO/scripts/link-agent-context.sh"
LINK="$REPO/scripts/link-agent-context.sh"

# Disposable sibling worktree (tracked content only).
git -C "$REPO" worktree add -q -b feature/link-ctx "$WT" HEAD

# --- help must not leak shell options ---
help_out="$("$LINK" --help)"
printf '%s\n' "$help_out" | grep -q 'link-agent-context' || fail "help missing title"
printf '%s\n' "$help_out" | grep -q 'set -euo pipefail' && fail "help prints set -euo pipefail" || true
pass "help omits shell options"

# --- attach exact symlinks ---
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null
for name in AGENTS.md CLAUDE.md RTK.md; do
  [[ -L "$WT/$name" ]] || fail "expected symlink $WT/$name"
  [[ "$(readlink "$WT/$name")" == "$REPO/$name" ]] || fail "wrong target for $name"
done
[[ ! -e "$WT/.projectmem" ]] || fail "worktree must not host .projectmem"
pass "exact doctrine symlinks"

# --- idempotent re-link ---
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null
pass "idempotent re-link"

# --- status OK when exact ---
"$LINK" --from "$REPO" --worktree "$WT" --status >/dev/null
pass "status clean for exact links"

# --- status flags wrong source symlink ---
rm -f "$WT/AGENTS.md"
ln -s /tmp/wrong-agents-doctrine "$WT/AGENTS.md"
set +e
status_out="$("$LINK" --from "$REPO" --worktree "$WT" --status 2>&1)"
status_rc=$?
set -e
[[ "$status_rc" -ne 0 ]] || fail "status should fail on wrong symlink"
printf '%s\n' "$status_out" | grep -q 'ERROR symlink' || fail "status should report ERROR symlink"
pass "status flags wrong doctrine source"
# repair
rm -f "$WT/AGENTS.md"
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null

# --- refuse worktree-local .projectmem before links ---
rm -f "$WT/AGENTS.md" "$WT/CLAUDE.md" "$WT/RTK.md"
mkdir -p "$WT/.projectmem"
set +e
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null 2>"$TEST_HOME/pm.err"
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "should refuse .projectmem"
grep -q 'ProjectMem stays canonical-only' "$TEST_HOME/pm.err" || fail "missing projectmem refuse message"
[[ ! -e "$WT/AGENTS.md" ]] || fail "partial doctrine after projectmem refuse"
rm -rf "$WT/.projectmem"
pass "refuse worktree .projectmem"

# --- refuse real-file overwrite without partial links ---
printf 'real\n' >"$WT/RTK.md"
set +e
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null 2>"$TEST_HOME/real.err"
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "should refuse real file"
[[ ! -e "$WT/AGENTS.md" ]] || fail "partial link after real-file refuse"
[[ ! -e "$WT/CLAUDE.md" ]] || fail "partial link after real-file refuse"
rm -f "$WT/RTK.md"
pass "atomic refuse on real-file conflict"

# --- refuse subdirectory destination ---
mkdir -p "$WT/scripts"
set +e
"$LINK" --from "$REPO" --worktree "$WT/scripts" >/dev/null 2>"$TEST_HOME/sub.err"
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "should refuse subdirectory"
grep -q 'worktree root' "$TEST_HOME/sub.err" || fail "missing worktree-root message"
pass "refuse subdirectory destination"

# --- bulk continues after one failure ---
WT2="$TEST_HOME/wt2"
git -C "$REPO" worktree add -q -b feature/link-ctx-2 "$WT2" HEAD
mkdir -p "$WT/.projectmem"
set +e
"$LINK" --from "$REPO" --all-worktrees >"$TEST_HOME/all.out" 2>"$TEST_HOME/all.err"
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "bulk should fail when one worktree is poisoned"
[[ -L "$WT2/AGENTS.md" ]] || fail "bulk should still attach healthy worktree"
grep -q 'FAILED' "$TEST_HOME/all.out" "$TEST_HOME/all.err" || fail "bulk should report FAILED"
rm -rf "$WT/.projectmem"
pass "bulk continues after per-worktree failure"

# --- porcelain privacy after attach ---
"$LINK" --from "$REPO" --worktree "$WT" >/dev/null
dirty="$(git -C "$WT" status --porcelain -- AGENTS.md CLAUDE.md RTK.md .projectmem)"
[[ -z "$dirty" ]] || fail "doctrine paths dirty after attach: $dirty"
pass "linked doctrine stays ignored"

printf 'All link-agent-context contracts passed.\n'
