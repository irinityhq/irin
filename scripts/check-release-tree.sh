#!/usr/bin/env bash
# Verify that a release snapshot contains the product and excludes private work.
set -euo pipefail

if ! command -v rg >/dev/null 2>&1; then
  printf 'FAIL: release-tree checker requires ripgrep (rg)\n' >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

failures=0
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

required=(
  .github/CODEOWNERS
  .github/ISSUE_TEMPLATE/bug_report.yml
  .github/ISSUE_TEMPLATE/config.yml
  .github/ISSUE_TEMPLATE/feature_request.yml
  .github/ISSUE_TEMPLATE/question.yml
  .github/dependabot.yml
  AGENTS.md
  Cargo.toml
  CODE_OF_CONDUCT.md
  CONTRIBUTING.md
  README.md
  SUPPORT.md
  docs/architecture.md
  docs/cabinets.md
  docs/ci-operating-model.md
  docs/development-workflow.md
  docs/security-claims-vs-reality.md
  docs/troubleshooting.md
  council-rs/scripts/warroom-tauri-dev.sh
  council-rs/warroom/web/playwright.export.config.ts
  scripts/check-contract-surface-declaration.sh
  scripts/bootstrap-actionlint.sh
  scripts/bootstrap-dev-tools.sh
  scripts/dev-check.sh
  scripts/dev-preflight.sh
  scripts/gortex-worktree.sh
  scripts/with-test-ports.sh
  scripts/macos-window-proof.swift
  scripts/new-worktree.sh
  scripts/remove-worktree.sh
  scripts/smoke-macos-tauri-app.sh
  scripts/test-development-workflow.sh
  scripts/irin-runtime.sh
  scripts/setup-local.sh
  council-rs/src/main.rs
  council-rs/warroom/web/package-lock.json
  council-rs/warroom/web/app/page.tsx
  council-rs/warroom-tauri/src-tauri/Cargo.toml
  gateway/docker-compose.yml
  gateway/docker-compose.canary.yml
  gateway/sidecar-rs/src/main.rs
  gateway/sidecar-rs/src/watch/registry.rs
  sentinel/COMMS_CONTRACT.md
  sentinel/sovereign-protocol/Cargo.toml
)

for path in "${required[@]}"; do
  [[ -f "$path" ]] || fail "required product path missing: $path"
done

for sentinel in \
  anomaly completion_verify file_inbox queue_depth ledger_delta \
  precedent_integrity silence watch_health; do
  [[ -f "gateway/sidecar-rs/src/watch/sentinels/${sentinel}.rs" ]] \
    || fail "Sentinel implementation missing: $sentinel"
done

files=()
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  while IFS= read -r path; do files[${#files[@]}]="$path"; done < <(git ls-files)
else
  while IFS= read -r path; do files[${#files[@]}]="$path"; done \
    < <(find . \
      -type d \( -name target -o -name node_modules -o -name .next \
        -o -name out -o -name dist -o -name build -o -name coverage \
        -o -name .turbo -o -name .vercel -o -name warroom-web-dist \
        -o -name __pycache__ \) -prune \
      -o -type f -not -name '*.tsbuildinfo' -print \
      | sed 's#^./##' | sort)
fi

for path in "${files[@]}"; do
  [[ "$path" == "build.rs" || "$path" == */build.rs ]] || continue
  grep -Fqx "/$path @iws17" .github/CODEOWNERS \
    || fail "build-time authority path lacks explicit CODEOWNERS ownership: $path"
done

for path in "${files[@]}"; do
  case "/$path/" in
    */CLAUDE.md/*|*/RTK.md/*|*/.docs-lab/*|*/.codex/*|*/.claude/*|*/.cursor/*|*/.grok/*|*/.gortex.yaml/*|*/.mcp.json/*|*/state/*|*/sessions/*|*/runs/*|*/librarian_chats/*|*/warroom-ios/*|*/docs/media/*|*/docs/audits/*|*/docs/plans/*|*/docs/specs/*|*/eval/run-*|*/target/*|*/node_modules/*|*/.next/*|*/out/*|*/dist/*|*/build/*|*/coverage/*|*/.turbo/*|*/.vercel/*|*/warroom-web-dist/*|*/__pycache__/*|*.tsbuildinfo/*|*/playwright-report/*|*/test-results/*)
      fail "private or generated path included: $path"
      ;;
  esac

  # Only the root AGENTS.md is the public repository operating manual; any
  # nested AGENTS.md is treated as private agent instruction material.
  case "$path" in
    AGENTS.md) ;;
    */AGENTS.md) fail "private or generated path included: $path" ;;
  esac

  case "$path" in
    *.md)
      case "$path" in
        .github/PULL_REQUEST_TEMPLATE.md|AGENTS.md|CODE_OF_CONDUCT.md|CONTRIBUTING.md|README.md|SECURITY.md|SUPPORT.md|\
        council-rs/README.md|council-rs/SECURITY.md|\
        council-rs/docs/operator-guide.md|council-rs/docs/persistence.md|\
        council-rs/docs/providers.md|council-rs/docs/war-room.md|\
        council-rs/warroom-tauri/README.md|council-rs/warroom/docs/TAURI-AUTH.md|\
        docs/architecture.md|docs/cabinets.md|docs/ci-operating-model.md|docs/development-workflow.md|docs/release-runbook.md|docs/security-claims-vs-reality.md|\
        docs/surface-map.md|docs/troubleshooting.md|\
        packaging/gateway-pack/README.md|\
        gateway/COUNCIL_GATEWAY_CONTRACT.md|gateway/README.md|gateway/SECURITY.md|\
        gateway/docs/gateway-core-surfaces.md|gateway/docs/operator-quickstart.md|gateway/docs/runbook.md|\
        gateway/docs/runbooks/arming-authorization.md|gateway/docs/verify.md|\
        gateway/docs/watch-api.md|gateway/docs/watch-plane-retention.md|\
        sentinel/COMMS_CONTRACT.md|sentinel/README.md|sentinel/SECURITY.md|\
        sentinel/docs/protocol-implementation.md|sentinel/docs/YOUR-AGENT.md|sentinel/docs/runbooks/arm-producer.md|\
        sentinel/docs/runbooks/disarm-producer.md)
          ;;
        *) fail "documentation path requires release allowlist review: $path" ;;
      esac
      ;;
  esac

  case "$path" in
    *.png|*.PNG|*.jpg|*.JPG|*.jpeg|*.JPEG|*.gif|*.GIF|*.webp|*.WEBP|\
    *.svg|*.SVG|*.ico|*.ICO|*.icns|*.ICNS|*.pdf|*.PDF|*.heic|*.HEIC|\
    *.mov|*.MOV|*.mp4|*.MP4|*.mp3|*.MP3|*.m4a|*.M4A|*.wav|*.WAV|\
    *.zip|*.ZIP|*.tgz|*.TGZ|*.dmg|*.DMG|*.pkg|*.PKG)
      case "$path" in
        assets/readme/chair-ruling.png|assets/readme/seat-stream.png|\
        assets/readme/sheldon-validation.png|assets/readme/warroom-deliberation.gif|\
        assets/readme/warroom-walkthrough.mp4|\
        council-rs/warroom-tauri/src-tauri/icons/*) ;;
        *) fail "media or packaged artifact requires release allowlist review: $path" ;;
      esac
      ;;
  esac
done

if rg -n -I -e '/(Users|home)/[[:alnum:]_.-]+/' . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' \
  --glob '!scripts/check-release-tree.sh' --glob '!scripts/check-public-pr-language.sh'; then
  fail "personal home path found"
fi

if rg -n -I -e '~/(Projects|Documents|Desktop|Downloads)/|\.zshrc' . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' --glob '!scripts/check-release-tree.sh'; then
  fail "personal workspace or shell-startup path found"
fi

review_metadata='Greptile|Copilot review|Codex (P[0-9]|finding|review|catch)|codex (round|caught)|design SHA|closeout review|pre-public pass|review fix|ruling #[0-9]|(pre|post)-PR-[A-Z]|Council `[0-9a-f-]{7,}`|warroom P[0-9]|review (condition|action|P[0-9])|pending-retry review|follow-on #[0-9]+|grok (finding|re-review|-adversarial)|H[0-9]+-followup|rc[0-9]-[a-z]'
if rg -n -I -i -e "$review_metadata" . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' --glob '!scripts/check-release-tree.sh'; then
  fail "review or implementation-history metadata found"
fi

dated_process='(closeout|decision|incident|pre-public|red.?team).{0,24}20[0-9]{2}-[0-9]{2}-[0-9]{2}|20[0-9]{2}-[0-9]{2}-[0-9]{2}.{0,24}(incident|pre-public|red.?team)'
if rg -n -I -i -e "$dated_process" . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' --glob '!scripts/check-release-tree.sh'; then
  fail "dated internal process narrative found"
fi

if rg -n -I -e 'docs/specs/' . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' --glob '!scripts/check-release-tree.sh'; then
  fail "reference to excluded internal specs found"
fi

trusted_pr_runner_line="runs-on: \${{ fromJSON(github.event.repository.private == true && (github.event_name != 'pull_request' || (github.event.pull_request.head.repo.full_name == github.repository && github.event.pull_request.user.login == 'iws17')) && '{\"group\":\"IRIN trusted main\",\"labels\":\"irin-ci\"}' || '[\"ubuntu-latest\"]') }}"
trusted_proof_runner_line="runs-on: \${{ fromJSON(github.event.repository.private == true && '{\"group\":\"IRIN trusted main\",\"labels\":\"irin-ci\"}' || '[\"ubuntu-latest\"]') }}"
if rg -n -I 'runs-on:' .github/workflows \
  | sed -E 's/^[0-9]+:[[:space:]]*//' \
  | grep -Fvx -e 'runs-on: ubuntu-latest' -e 'runs-on: macos-15' -e "$trusted_pr_runner_line" -e "$trusted_proof_runner_line"; then
  fail "CI workflow uses a nonstandard runner; review before release"
fi

if rg -n -I -e '[A-Za-z0-9-]+\.ts\.net' . \
  --glob '!**/target/**' --glob '!**/node_modules/**' \
  --glob '!**/.next/**' --glob '!**/out/**' --glob '!**/dist/**' --glob '!**/build/**' --glob '!**/coverage/**' \
  --glob '!**/.turbo/**' --glob '!**/.vercel/**' --glob '!**/warroom-web-dist/**' --glob '!**/__pycache__/**' \
  --glob '!**/*.tsbuildinfo' --glob '!scripts/check-release-tree.sh' \
  | grep -v 'example\.ts\.net'; then
  fail "private Tailscale hostname found"
fi

for attributes in council-rs/.gitattributes gateway/.gitattributes sentinel/.gitattributes; do
  if [[ -f "$attributes" ]] && grep -Eq '^warroom/?[[:space:]]+export-ignore' "$attributes"; then
    fail "$attributes excludes the War Room from exports"
  fi
done

if (( failures > 0 )); then
  printf '%d release-tree check(s) failed\n' "$failures" >&2
  exit 1
fi

printf 'release tree: OK (%d files checked)\n' "${#files[@]}"
