.DEFAULT_GOAL := help

.PHONY: help release-check worktree worktree-remove tools lint-crypto preflight check ship-check verify verify-down verify-formal docker-cache-prune warroom dmg-build dmg-verify dmg-smoke build test gateway-pack-stage gateway-pack-dev-images gateway-pack-test gateway-pack-integration-smoke gateway-pack-ui-smoke gateway-pack-prod-images production-manifest release-transaction candidate-status install-verify record-acceptance export-candidate import-candidate link-ship-board worktree-gc lint-security opengrep lint-lua gateway-prepare-config cli-proxies-up cli-proxies-down cli-proxies-status

release-check: ## Verify public source-tree boundaries
	bash scripts/check-public-tree.sh

worktree: ## Create an isolated development worktree (BRANCH=feature/example)
	@test -n "$(BRANCH)" || (echo "usage: make worktree BRANCH=feature/example"; exit 2)
	bash scripts/new-worktree.sh "$(BRANCH)" "$(DEST)"

worktree-remove: ## Stop, untrack, and remove a clean development worktree (DEST=/path)
	@test -n "$(DEST)" || (echo "usage: make worktree-remove DEST=/absolute/path/to/worktree"; exit 2)
	bash scripts/remove-worktree.sh "$(DEST)"

worktree-gc: ## List (or APPLY=1 remove) clean worktrees already merged into origin/main
	@if [ "$(APPLY)" = "1" ]; then bash scripts/worktree-gc.sh --apply; else bash scripts/worktree-gc.sh; fi

tools: ## Install checksum-verified ship tools into ignored repo-local state
	bash scripts/bootstrap-dev-tools.sh
	bash scripts/bootstrap-actionlint.sh

lint-crypto: ## Advisory IRIN crypto dylints (IRIN_DYLINT_FAIL=1 to gate)
	bash scripts/run-dylint.sh

lint-security: ## Advisory Opengrep scan of critical product-security paths
	bash scripts/run-opengrep.sh

opengrep: lint-security ## Alias for lint-security

lint-lua: ## Advisory Selene lint of gateway OpenResty Lua (IRIN_SELENE_FAIL=1 to gate)
	bash scripts/run-selene.sh

preflight: ## Prove branch, base, and worktree isolation before editing
	bash scripts/dev-preflight.sh

check: ## Run fast tests selected from the current diff
	bash scripts/dev-check.sh

ship-check: ## Run the complete diff-selected product proof and emit a receipt
	bash scripts/dev-check.sh --ship

verify: ## Prove the loop ($0, no keys): one signed directive lands in the outbox
	$(MAKE) -C gateway verify

verify-down: ## Tear down the isolated verification stack and its local state
	$(MAKE) -C gateway verify-down

verify-formal: ## Selective Kani + Miri on sovereign-protocol JCS (advisory if tools missing)
	bash scripts/run-kani.sh
	bash scripts/run-miri.sh

gateway-prepare-config: ## Developer-only: prepare private Gateway local config (no services)
	bash gateway/tools/prepare-local-config.sh

docker-cache-prune: ## Reclaim rebuildable Docker BuildKit cache (keeps images, containers, and volumes)
	@docker info >/dev/null 2>&1 || (echo "The Docker daemon is not ready; start it before pruning the build cache."; exit 1)
	docker builder prune --all --force

warroom: ## macOS/Ubuntu: run Council + War Room Web in the foreground
	@set -a; test ! -f .irin-worktree.env || . ./.irin-worktree.env; set +a; \
	COUNCIL_PORT="$${IRIN_COUNCIL_PORT:-8765}" WARROOM_WEB_PORT="$${IRIN_WEB_PORT:-3010}" \
		$(MAKE) -C council-rs warroom-browser

dmg-build: ## Build IRIN candidate into IRIN_CANDIDATE_ROOT (prints candidate_path=)
	bash packaging/build-dmg.sh

dmg-verify: ## Verify named candidate (requires IRIN_CANDIDATE_PATH; never re-signs)
	bash packaging/verify-dmg.sh

dmg-smoke: ## Full-app smoke from named candidate (requires IRIN_CANDIDATE_PATH; always fresh-extract)
	bash packaging/smoke-full-app.sh

gateway-pack-stage: ## Stage runtime-only Gateway Pack into Tauri resources (gitignored)
	bash scripts/stage-gateway-pack.sh

gateway-pack-dev-images: ## Build local arm64 Gateway/sidecar images + test-only digest manifest
	bash scripts/build-gateway-pack-dev-images.sh

gateway-pack-test: ## Static + isolation tests for the optional Gateway Pack
	bash scripts/test-gateway-pack-assets.sh
	bash scripts/test-gateway-pack-isolation.sh
	bash scripts/test-gateway-pack-desktop-ownership.sh
	bash packaging/test-candidate-store.sh
	bash scripts/test-candidate-status.sh
	bash scripts/test-release-transaction-w3.sh
	bash scripts/test-export-import-candidate.sh
	bash scripts/test-classify-ci-paths.sh

gateway-pack-integration-smoke: ## Isolated compose smoke (local-dev images; foreign fixtures survive the product, harness cleans its own)
	bash scripts/test-gateway-pack-integration-smoke.sh

gateway-pack-ui-smoke: ## Local-dev packaged UI lifecycle smoke (requires copied app, dev images, free ports, Accessibility)
	bash packaging/smoke-gateway-pack.sh

gateway-pack-prod-images: ## Build + push production GHCR images (IRIN_PACK_IMAGES_TAG=vX.Y.Z|rc-<sha>)
	bash scripts/build-gateway-pack-prod-images.sh

production-manifest: ## Pin production manifest from live GHCR digests (IRIN_PACK_IMAGES_TAG=...)
	bash scripts/generate-production-manifest.sh

candidate-status: ## Sole candidate-tier reporter (ARGS=--candidate PATH [--json] [--require TIER])
	bash scripts/candidate-status.sh $(ARGS)

install-verify: ## Fresh-extract candidate DMG into install/ + write install proof (ARGS=--candidate PATH)
	bash scripts/install-verify-candidate.sh $(ARGS)

export-candidate: ## Deterministic candidate archive + sha256 sidecar (ARGS=--candidate PATH [--output DIR])
	bash scripts/export-candidate.sh $(ARGS)

import-candidate: ## Verify archive and atomically import into IRIN_CANDIDATE_ROOT (ARGS=--archive PATH [...])
	bash scripts/import-candidate.sh $(ARGS)

record-acceptance: ## Interactive T2 acceptance (ARGS=--candidate PATH --installed-app PATH; tty required)
	bash scripts/record-acceptance.sh $(ARGS)

release-transaction: ## Prepare or publish (ARGS=--prepare-production --t1-packet P | --publish --tag vX.Y.Z --candidate PATH --t2-packet PATH)
	bash scripts/release-transaction.sh $(ARGS)

link-ship-board: ## Link durable ship-board into this worktree (operator-owned SSOT)
	bash scripts/link-ship-board.sh

build: ## Build the full Rust workspace in release mode
	bash scripts/cargo-target-policy.sh run "$(CURDIR)" cargo build --workspace --release

test: ## Run the full Rust workspace test suite
	bash scripts/cargo-target-policy.sh run "$(CURDIR)" cargo test --workspace

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "Targets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ { printf "  %-14s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
