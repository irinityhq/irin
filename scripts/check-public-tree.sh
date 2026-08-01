#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run this check from an IRIN Git checkout\n' >&2
  exit 2
}
cd "$ROOT"

command -v rg >/dev/null 2>&1 || {
  printf 'ERROR: ripgrep (rg) is required\n' >&2
  exit 2
}

failures=0
checked=0
fail() {
  printf 'ERROR: %s\n' "$1" >&2
  failures=1
}

required_paths=(
  Cargo.toml
  README.md
  SECURITY.md
  council-rs/Cargo.toml
  gateway/README.md
  sentinel/sovereign-protocol/Cargo.toml
)
for path in "${required_paths[@]}"; do
  [[ -f "$path" ]] || fail "required public product path is missing: $path"
done

while IFS= read -r path; do
  [[ -e "$path" || -L "$path" ]] || continue
  checked=$((checked + 1))
  case "$path" in
    AGENTS.md|*/AGENTS.md|CLAUDE.md|*/CLAUDE.md|GEMINI.md|*/GEMINI.md|RTK.md|*/RTK.md|YOUR-AGENT.md|*/YOUR-AGENT.md)
      fail "private assistant instruction file is tracked: $path"
      ;;
    .hermes/*|*/.hermes/*|.zcode/*|*/.zcode/*|.codex/*|*/.codex/*|.claude/*|*/.claude/*|.cursor/*|*/.cursor/*|.grok/*|*/.grok/*|.irin-receipts/*|*/.irin-receipts/*)
      fail "private workspace or receipt path is tracked: $path"
      ;;
    sessions/*|*/sessions/*|runs/*|*/runs/*|librarian_chats/*|*/librarian_chats/*)
      fail "generated runtime state is tracked: $path"
      ;;
    .gortex.yaml|*/.gortex.yaml|.mcp.json|*/.mcp.json|greptile.json|*/greptile.json)
      fail "private tool configuration is tracked: $path"
      ;;
    docs/*-execution.md|docs/*-execution-record.md|docs/*-simplification-plan.md|docs/audits/*|docs/plans/*)
      fail "internal execution or planning record is tracked: $path"
      ;;
  esac
done < <(git ls-files)

while IFS= read -r build_script; do
  [[ -e "$build_script" ]] || continue
  if ! rg -Fq -- "/$build_script" .github/CODEOWNERS; then
    fail "build-time execution surface lacks an explicit CODEOWNERS entry: $build_script"
  fi
done < <(git ls-files '*build.rs')

private_tool_a='Gor'"tex"
private_tool_b='Grep'"tile"
private_patterns="/Users/[A-Za-z0-9._-]+/(Projects|Documents|Desktop)/|/home/[A-Za-z0-9._-]+/(Projects|Documents|Desktop)/|Claude-Session:|claude\.ai/code/session|${private_tool_a}|${private_tool_b}|compagents/|ship-20[0-9]{6}[^[:space:]]*\.txt"
set +e
matches="$(git grep -nI -E "$private_patterns" -- \
  '*.md' '*.yml' '*.yaml' '*.json' 'Makefile' 'scripts/*.sh' \
  ':(exclude)scripts/check-public-tree.sh' \
  ':(exclude)scripts/check-public-pr-language.sh' 2>/dev/null)"
grep_status=$?
set -e
case "$grep_status" in
  0)
    printf '%s\n' "$matches" >&2
    fail "private workflow or machine-local content is present in the public tree"
    ;;
  1)
    ;;
  *)
    fail "public-tree content scan could not complete"
    ;;
esac

if [[ "$failures" -ne 0 ]]; then
  exit 1
fi

printf 'public tree: OK (%d tracked files checked)\n' "$checked"
