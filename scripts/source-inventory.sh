#!/usr/bin/env bash
# Source inventory for the simplification program.
#
# Pins the counting rules used to report net line changes across the
# simplification PR series, so every "net lines removed" figure is computed
# the same way before and after. Each tracked file lands in exactly one
# category; the first matching rule wins:
#
#   lockfiles   dependency lockfiles
#   docs        markdown prose and licenses
#   tests       test code, test trees, fixtures, and proof harnesses
#   tooling     build/CI/scripts, benches, examples, manifests, repo config
#   production  first-party product source: Rust, TypeScript, Lua, seat
#               prompt templates, and runtime-loaded product data (gateway
#               conf, model registry, cabinets)
#   other       non-text assets, excluded from line figures
#
# Headline figures (what the program reports per PR):
#   production        production lines
#   first-party       production + tests + tooling
#   total text        all tracked text files
#
# Usage:
#   scripts/source-inventory.sh            # tables + per-category rollup
#   scripts/source-inventory.sh --json     # machine-readable, for PR diffs
#
# Run on a clean tree; the emitted revision names the counted commit.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

BINARY_EXTS='png ico icns jpg jpeg gif webp woff woff2 ttf otf eot mp4 mov zip tar gz dylib a so bin der'

classify() {
  local path="$1" base
  base="${path##*/}"

  case "$base" in
    Cargo.lock | package-lock.json | *.lock)
      printf 'lockfiles\n'; return ;;
  esac

  case "$base" in
    *.md | LICENSE* | COPYING*)
      printf 'docs\n'; return ;;
  esac

  case "$base" in
    *_test.rs | test_*.rs | *_test.lua | test_*.lua | \
    *.test.ts | *.test.tsx | *.spec.ts | *.spec.tsx | \
    test-*.sh | test_*.py | *_test.py | *.stderr | *.stdout)
      printf 'tests\n'; return ;;
  esac
  case "$path" in
    tests/* | */tests/* | fixtures/* | */fixtures/* | \
    */__fixtures__/* | */__snapshots__/* | gateway/test*)
      printf 'tests\n'; return ;;
  esac

  case "$path" in
    scripts/* | */scripts/* | .github/* | tools/* | */tools/* | \
    packaging/* | */packaging/* | gateway/bin/* | gateway/demo/* | \
    benches/* | */benches/* | .vscode/* | */.vscode/* | dev/* | demo/* | \
    examples/* | */examples/*)
      printf 'tooling\n'; return ;;
  esac

  # Product runtime data: config the shipped Gateway and Council actually
  # load (policies, model registry, cabinets, prompt-adjacent allowlists).
  # Operator templates (*.example) stay tooling.
  case "$path" in
    gateway/conf/* | gateway/nginx.conf | council-rs/config/* | \
    council-rs/cabinets/*)
      case "$base" in
        *.example) ;;
        *) printf 'production\n'; return ;;
      esac
      ;;
  esac
  case "$path" in
    council-rs/*.yaml)
      printf 'production\n'; return ;;
  esac

  case "$base" in
    Makefile | Makefile.* | *.mk | Cargo.toml | *.toml | *.yaml | *.yml | \
    *.json | *.config.* | .gitignore | .gitattributes | .gitkeep | \
    .editorconfig | .gitleaks.toml | .grokignore | *.example | Dockerfile* | \
    *.dockerignore)
      printf 'tooling\n'; return ;;
  esac

  case "$path" in
    council-rs/* | gateway/lua/* | gateway/conf/* | gateway/sidecar-rs/* | sentinel/*)
      printf 'production\n'; return ;;
  esac

  printf 'other\n'
}

is_text() {
  local base="$1" ext
  case "$base" in
    *.*)
      ext="${base##*.}"
      case " $BINARY_EXTS " in
        *" $ext "*) return 1 ;;
      esac
      ;;
  esac
  return 0
}

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

rev="$(git rev-parse HEAD)"

while IFS= read -r -d '' path; do
  cat="$(classify "$path")"
  base="${path##*/}"
  if is_text "$base"; then
    lines="$(wc -l < "$path" | tr -d ' ')"
  else
    lines=-
  fi
  printf '%s\t%s\t%s\n' "$cat" "$lines" "$path"
done < <(git ls-files -z) > "$tmp"

rollup() {
  awk -F'\t' -v want="$1" '
    $2 != "-" && $1 == want { sum += $2 }
    $1 == want { files += 1 }
    END { printf "%d\t%d\n", files, sum }
  ' "$tmp"
}

figure() {
  awk -F'\t' -v want_re="$1" '
    $2 != "-" && $1 ~ want_re { sum += $2 }
    END { printf "%d\n", sum }
  ' "$tmp"
}

if [[ "${1:-}" == "--json" ]]; then
  printf '{\n'
  printf '  "revision": "%s",\n' "$rev"
  for cat in production tests tooling docs lockfiles other; do
    read -r files lines <<< "$(rollup "$cat")"
    printf '  "%s": { "files": %d, "lines": %s },\n' \
      "$cat" "$files" "${lines/-/0}"
  done
  printf '  "figures": { "production": %s, "first_party": %s, "total_text": %s }\n' \
    "$(figure '^production$')" \
    "$(figure '^(production|tests|tooling)$')" \
    "$(figure '^(production|tests|tooling|docs|lockfiles)$')"
  printf '}\n'
  exit 0
fi

printf '== source inventory @ %s ==\n\n' "$rev"
printf '%-12s %6s %9s\n' category files lines
for cat in production tests tooling docs lockfiles other; do
  read -r files lines <<< "$(rollup "$cat")"
  printf '%-12s %6d %9s\n' "$cat" "$files" "$lines"
done
printf '\nheadline figures (lines):\n'
printf '  production   %10s\n' "$(figure '^production$')"
printf '  first-party  %10s  (production + tests + tooling)\n' \
  "$(figure '^(production|tests|tooling)$')"
printf '  total text   %10s  (+ docs + lockfiles)\n' \
  "$(figure '^(production|tests|tooling|docs|lockfiles)$')"
printf '\nper-category subtree rollup:\n'
awk -F'\t' '
  {
    n = split($3, seg, "/")
    top = seg[1]
    if (n > 1) top = top "/" seg[2]
    key = $1 " " top
    files[key] += 1
    if ($2 != "-") lines[key] += $2
  }
  END {
    for (key in files)
      printf "%s\t%d\t%d\n", key, files[key], lines[key]
  }
' "$tmp" | sort | awk -F'\t' '{ printf "  %-34s %5d %8d\n", $1, $2, $3 }'
