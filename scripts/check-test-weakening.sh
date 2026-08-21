#!/usr/bin/env bash
# Test-weakening tripwire. Compares the working tree (and untracked files)
# against a base ref and names every change that lowers the proof bar:
#   deleted test files, removed test cases, added skips/ignores, net assertion
#   loss in test files, escape hatches added to proof scripts, raised
#   timeouts/retry counts, and source changes with no test change at all.
# Any finding exits 1. The only override is an explicit, non-empty
# IRIN_TEST_WEAKENING_ACK="<reason>", which still prints every finding. CI
# takes the reason from a "Test-weakening-ack: <reason>" line in the PR body.
#
# usage: scripts/check-test-weakening.sh [base-ref]   (default origin/main)
set -euo pipefail

base="${1:-origin/main}"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: test-weakening tripwire must run inside a Git checkout\n' >&2
  exit 1
}
cd "$root"
git rev-parse --verify --quiet "${base}^{commit}" >/dev/null || {
  printf 'ERROR: base ref not found: %s\n' "$base" >&2
  exit 1
}

diff_file="$(mktemp "${TMPDIR:-/tmp}/irin-test-weakening.XXXXXX")"
trap 'rm -f "$diff_file"' EXIT
git diff -U0 --no-color --no-ext-diff "$base" -- >"$diff_file"
untracked="$(git ls-files --others --exclude-standard)"

IRIN_TW_ACK="${IRIN_TEST_WEAKENING_ACK:-}" \
  python3 - "$diff_file" "$untracked" <<'PY'
import os
import re
import sys

diff_path, untracked_raw = sys.argv[1], sys.argv[2]
ack = os.environ.get("IRIN_TW_ACK", "").strip()

TEST_FILE = re.compile(
    r"(^|/)tests?/.*\.(rs|sh|ts|tsx|js|lua)$"
    r"|_tests?\.rs$"
    r"|\.(test|spec)\.(ts|tsx|js)$"
    r"|(^|/)test-[^/]+\.sh$"
    r"|_spec\.lua$|_test\.lua$"
)
SOURCE_FILE = re.compile(r"\.(rs|ts|tsx|js|lua)$")
PROOF_FILE = re.compile(
    r"^(scripts/|packaging/|gateway/test/|\.github/workflows/).*\.(sh|ya?ml)$|(^|/)Makefile$|\.mk$"
)
TEST_DECL = re.compile(
    r"#\[(tokio::)?test\]|#\[test_case|#\[rstest"
    r"|^\s*(it|test|describe)(\.each)?\s*\("
    r"|^\s*(it|test)\.each\b"
    r"|^\s*(it|describe)\s*\(\s*['\"]"
)
SKIP = re.compile(
    r"#\[ignore"
    r"|\b(it|test|describe)\.(skip|todo)\s*\("
    r"|\bx(it|test|describe)\s*\("
    r"|\bpending\s*\("
)
ASSERT = re.compile(
    r"\bassert(_eq|_ne|_matches)?!"
    r"|\bexpect\s*\("
    r"|\bpanic!\s*\("
    r"|\bunwrap_err\b"
    r"|\bassert\.[a-zA-Z_]+\s*\("
    r"|\bfail\s+[\"']"
)
HATCH = re.compile(
    r"\|\|\s*true\b"
    r"|\|\|\s*:(\s|$)"
    r"|continue-on-error:\s*true"
    r"|--no-verify\b"
    r"|\bset\s\+e\b"
    r"|\b[A-Z_]*SKIP[A-Z_]*=1\b"
)
TUNABLE = re.compile(r"(?i)timeout|retr(y|ies)|attempt|_checks|sleep|deadline|max_wait|poll")
NUM = re.compile(r"\d+")
QUOTED = re.compile(r"'[^']*'|\"[^\"]*\"|`[^`]*`")
COMMENT = re.compile(r"^\s*#")
# Tolerated `|| true`: teardown verbs that must not abort cleanup paths.
CLEANUP = re.compile(
    r"^\s*(kill|pkill|rm|rmdir|wait|osascript|trap|unset|lsof|umount|hdiutil detach"
    r"|docker\s+(rm|stop|kill|compose\s+down)|git\s+worktree\s+remove|tmutil|launchctl\s+(bootout|unload))\b"
)

removed = {}   # file -> [line]
added = {}     # file -> [(lineno, line)]
deleted_files = []
changed_files = set()

cur = None
new_line = 0
with open(diff_path, encoding="utf-8", errors="replace") as handle:
    for raw in handle:
        line = raw.rstrip("\n")
        if line.startswith("--- "):
            continue
        if line.startswith("+++ "):
            target = line[4:]
            if target == "/dev/null":
                cur = None
                continue
            cur = target[2:] if target.startswith("b/") else target
            changed_files.add(cur)
            removed.setdefault(cur, [])
            added.setdefault(cur, [])
            continue
        if line.startswith("diff --git"):
            # Deleted files never get a +++ b/ header; capture them here.
            m = re.match(r"diff --git a/(.*) b/(.*)$", line)
            pending = m.group(2) if m else None
            cur = None
            continue
        if line.startswith("deleted file mode"):
            if pending:
                deleted_files.append(pending)
                changed_files.add(pending)
                removed.setdefault(pending, [])
                added.setdefault(pending, [])
                cur = pending
            continue
        if line.startswith("@@"):
            m = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)", line)
            new_line = int(m.group(1)) if m else 0
            continue
        if cur is None:
            continue
        if line.startswith("+"):
            added[cur].append((new_line, line[1:]))
            new_line += 1
        elif line.startswith("-"):
            removed[cur].append(line[1:])

for path in [p for p in untracked_raw.split("\n") if p]:
    if path in changed_files or not os.path.isfile(path):
        continue
    changed_files.add(path)
    removed.setdefault(path, [])
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            added[path] = [(i + 1, l.rstrip("\n")) for i, l in enumerate(handle)]
    except OSError:
        added[path] = []

findings = []

def add(kind, path, lineno, detail):
    findings.append((kind, path, lineno, detail))

# 1. Deleted test files.
for path in deleted_files:
    if TEST_FILE.search(path):
        add("deleted-test-file", path, 0, "test file removed")

# 2. Removed test cases (net), 3. added skips, 4. net assertion loss.
for path in sorted(changed_files):
    rem = removed.get(path, [])
    addl = added.get(path, [])
    rem_decl = sum(1 for l in rem if TEST_DECL.search(l))
    add_decl = sum(1 for l in addl if TEST_DECL.search(l[1]))
    if rem_decl > add_decl and path not in deleted_files:
        add("removed-test-cases", path, 0, f"{rem_decl - add_decl} test case(s) removed net")
    if SOURCE_FILE.search(path):
        for lineno, l in addl:
            if SKIP.search(l):
                add("added-skip", path, lineno, l.strip())
    if TEST_FILE.search(path) or rem_decl or add_decl:
        rem_assert = sum(len(ASSERT.findall(l)) for l in rem)
        add_assert = sum(len(ASSERT.findall(l[1])) for l in addl)
        if rem_assert > add_assert and path not in deleted_files:
            add("assertion-loss", path, 0, f"{rem_assert - add_assert} assertion(s) removed net")

# 5. Escape hatches added to proof scripts. Test scripts carry fixtures and are
# covered by assertion-loss instead.
for path in sorted(changed_files):
    if not PROOF_FILE.search(path) or TEST_FILE.search(path):
        continue
    for lineno, l in added.get(path, []):
        if COMMENT.search(l):
            continue
        bare = QUOTED.sub("", l)
        if HATCH.search(bare) and not CLEANUP.search(bare):
            add("proof-escape-hatch", path, lineno, l.strip())

# 6. Raised timeouts / retry counts: same line text, larger number.
for path in sorted(changed_files):
    rem_tunable = {}
    for l in removed.get(path, []):
        if TUNABLE.search(l) and NUM.search(l):
            rem_tunable.setdefault(NUM.sub("#", l).strip(), []).append(max(int(n) for n in NUM.findall(l)))
    for lineno, l in added.get(path, []):
        if not (TUNABLE.search(l) and NUM.search(l)):
            continue
        key = NUM.sub("#", l).strip()
        if key in rem_tunable:
            old = max(rem_tunable[key])
            new = max(int(n) for n in NUM.findall(l))
            if new > old:
                add("raised-tunable", path, lineno, f"{old} -> {new}: {l.strip()}")

# 7. Source changed, no test changed anywhere.
src_changed = [p for p in changed_files if SOURCE_FILE.search(p) and not TEST_FILE.search(p) and p not in deleted_files]
test_touched = (
    any(TEST_FILE.search(p) for p in changed_files)
    or any(TEST_DECL.search(l[1]) or ASSERT.search(l[1]) for p in changed_files for l in added.get(p, []))
    or any(TEST_DECL.search(l) or ASSERT.search(l) for p in changed_files for l in removed.get(p, []))
)
if src_changed and not test_touched:
    add("source-without-tests", src_changed[0], 0,
        f"{len(src_changed)} source file(s) changed, no test file or test case touched")

if not findings:
    print("test-weakening: no findings")
    sys.exit(0)

for kind, path, lineno, detail in findings:
    loc = f"{path}:{lineno}" if lineno else path
    print(f"test-weakening: {kind} {loc} — {detail}")
print(f"test-weakening: {len(findings)} finding(s)")
if ack:
    print(f"test-weakening: acknowledged by IRIN_TEST_WEAKENING_ACK: {ack}")
    sys.exit(0)
sys.stdout.flush()
print("test-weakening: refusing. Locally set IRIN_TEST_WEAKENING_ACK=\"<reason>\"; "
      "in the PR description add a line \"Test-weakening-ack: <reason>\".", file=sys.stderr)
sys.exit(1)
PY
