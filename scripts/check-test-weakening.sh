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
untracked_file="$(mktemp "${TMPDIR:-/tmp}/irin-test-weakening-untracked.XXXXXX")"
trap 'rm -f "$diff_file" "$untracked_file"' EXIT
git diff -U0 --no-color --no-ext-diff "$base" -- >"$diff_file"
git ls-files --others --exclude-standard -z >"$untracked_file"

IRIN_TW_ACK="${IRIN_TEST_WEAKENING_ACK:-}" \
  python3 - "$diff_file" "$untracked_file" "$base" <<'PY'
import os
import re
import subprocess
import sys

diff_path, untracked_path, base_ref = sys.argv[1], sys.argv[2], sys.argv[3]
ack = os.environ.get("IRIN_TW_ACK", "").strip()

TEST_FILE = re.compile(
    r"(^|/)tests?/.*\.(rs|sh|ts|tsx|js|lua)$"
    r"|_tests?\.rs$"
    r"|(^|/)tests?\.rs$"
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

removed = {}   # file -> [(old_lineno, line)]
added = {}     # file -> [(new_lineno, line)]
deleted_files = []
changed_files = set()

cur = None
pending = None
new_line = 0
old_line = 0
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
            m = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)", line)
            old_line = int(m.group(1)) if m else 0
            new_line = int(m.group(2)) if m else 0
            continue
        if cur is None:
            continue
        if line.startswith("+"):
            added[cur].append((new_line, line[1:]))
            new_line += 1
        elif line.startswith("-"):
            removed[cur].append((old_line, line[1:]))
            old_line += 1

with open(untracked_path, "rb") as handle:
    untracked = [p.decode("utf-8", "replace") for p in handle.read().split(b"\0") if p]
for path in untracked:
    if path in changed_files or not os.path.isfile(path):
        continue
    # Only files a rule can see are worth reading; a stray log or video is not.
    if not (TEST_FILE.search(path) or SOURCE_FILE.search(path) or PROOF_FILE.search(path)):
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


# `#[cfg(test)]`, `#[cfg(all(test, ...))]`, `#[cfg(any(test, ...))]`,
# `#[cfg(all(feature = "x", test))]`. Never `#[cfg(not(test))]`: that is production.
CFG_TEST = re.compile(r"^\s*#\[cfg\((?:test\b|(?:all|any)\((?:[^()]*,\s*)?test\b)")
RAW_STR = re.compile(r"b?r(#*)\"")
CHAR_LIT = re.compile(r"'(?:\\.|[^\\'\n])'")

def code_braces(text):
    """Yield (line_index, brace) for every `{` / `}` in Rust source that is not
    inside a string, raw string, byte string, char literal, or comment."""
    i, n, line = 0, len(text), 0
    while i < n:
        c = text[i]
        word_start = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        if c == "\n":
            line += 1
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    line += text[i] == "\n"
                    i += 1
        elif word_start and (m := RAW_STR.match(text, i)):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
            line += text.count("\n", i, j)
            i = j
        elif c == '"' or (c == "b" and word_start and text.startswith('b"', i)):
            i += 2 if c == "b" else 1
            while i < n and text[i] != '"':
                if text[i] == "\\":
                    i += 1
                if i < n and text[i] == "\n":
                    line += 1
                i += 1
            i += 1
        elif c == "'" and (m := CHAR_LIT.match(text, i)):
            i = m.end()
        else:
            if c in "{}":
                yield line, c
            i += 1

def test_regions(text):
    """Line ranges (1-based, inclusive) of test-only items in Rust source:
    from a test `#[cfg]` attribute to the brace that closes its item. A region
    whose braces never balance runs to end of file."""
    lines = text.split("\n")
    starts = [i for i, l in enumerate(lines) if CFG_TEST.search(l)]
    if not starts:
        return []
    braces = list(code_braces(text))
    regions = []
    for start in starts:
        # A brace-less item (`mod tests;`, `use a::b;`) has no inline region:
        # `mod tests;` keeps its tests in a sibling file, which TEST_FILE covers.
        item = next((l for l in lines[start + 1:] if l.strip() and not l.lstrip().startswith("#[")), "")
        code = item.split("//")[0]
        if ";" in code and ("{" not in code or code.index(";") < code.index("{")):
            continue
        depth, end = 0, len(lines) - 1
        for li, ch in braces:
            if li < start:
                continue
            depth += 1 if ch == "{" else -1
            if depth <= 0:
                end = li
                break
        regions.append((start + 1, end + 1))
    return regions

def base_text(path):
    try:
        return subprocess.run(["git", "show", f"{base_ref}:{path}"], capture_output=True,
                              text=True, errors="replace", check=True).stdout
    except subprocess.CalledProcessError:
        return ""

def work_text(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""

def in_regions(lineno, regions):
    return any(a <= lineno <= b for a, b in regions)

# Lines that count as "test lines": every line of a TEST_FILE, or lines inside a
# `#[cfg(test)]` region of a Rust source file (old revision for removed lines,
# working tree for added lines). Production code never counts.
region_cache = {}
def test_lines(path):
    if path in region_cache:
        return region_cache[path]
    rem = removed.get(path, [])
    addl = added.get(path, [])
    if TEST_FILE.search(path):
        result = ([l for _, l in rem], [l for _, l in addl])
    elif path.endswith(".rs"):
        old_regions = test_regions(base_text(path))
        new_regions = test_regions(work_text(path))
        result = ([l for n, l in rem if in_regions(n, old_regions)],
                  [l for n, l in addl if in_regions(n, new_regions)])
    else:
        result = ([], [])
    region_cache[path] = result
    return result

# 1. Deleted test files.
for path in deleted_files:
    if TEST_FILE.search(path):
        add("deleted-test-file", path, 0, "test file removed")

# 2. Removed test cases (net), 3. added skips, 4. net assertion loss.
for path in sorted(changed_files):
    rem = removed.get(path, [])
    addl = added.get(path, [])
    rem_decl = sum(1 for _, l in rem if TEST_DECL.search(l))
    add_decl = sum(1 for _, l in addl if TEST_DECL.search(l))
    if rem_decl > add_decl and path not in deleted_files:
        add("removed-test-cases", path, 0, f"{rem_decl - add_decl} test case(s) removed net")
    if SOURCE_FILE.search(path):
        for lineno, l in addl:
            if SKIP.search(l):
                add("added-skip", path, lineno, l.strip())
    rem_t, add_t = test_lines(path)
    if rem_t or add_t:
        rem_assert = sum(len(ASSERT.findall(l)) for l in rem_t)
        add_assert = sum(len(ASSERT.findall(l)) for l in add_t)
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
    for _, l in removed.get(path, []):
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
test_touched = any(TEST_FILE.search(p) for p in changed_files) or any(
    TEST_DECL.search(l) or ASSERT.search(l)
    for p in changed_files
    for l in test_lines(p)[0] + test_lines(p)[1]
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
