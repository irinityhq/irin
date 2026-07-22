.DEFAULT_GOAL := help

.PHONY: help setup setup-prepare app-install release-check worktree verify verify-down runtime-up runtime-down runtime-restart runtime-status docker-cache-prune warroom warroom-tauri warroom-tauri-build build test

setup: ## macOS: prepare config, start the managed runtime, and enable login recovery
	bash scripts/setup-local.sh

setup-prepare: ## Prepare private local config and signing material without starting services
	bash scripts/setup-local.sh --prepare-only

app-install: ## Build, atomically install, and launch the Council War Room app
	bash scripts/install-macos-app.sh

release-check: ## Verify product completeness and tree hygiene
	bash scripts/check-release-tree.sh

worktree: ## Create an isolated development worktree (BRANCH=feature/example)
	@test -n "$(BRANCH)" || (echo "usage: make worktree BRANCH=feature/example"; exit 2)
	bash scripts/new-worktree.sh "$(BRANCH)" "$(DEST)"

verify: ## Prove the loop ($0, no keys): one signed directive lands in the outbox
	$(MAKE) -C gateway verify

verify-down: ## Tear down the isolated verification stack and its local state
	$(MAKE) -C gateway verify-down

runtime-up: ## Build and start the canonical local product runtime
	bash scripts/irin-runtime.sh start

runtime-down: ## Stop the canonical local product runtime
	bash scripts/irin-runtime.sh stop

runtime-restart: ## Rebuild and restart the canonical local product runtime
	bash scripts/irin-runtime.sh restart

runtime-status: ## Show Council, Web, Gateway, and Tailscale runtime status
	bash scripts/irin-runtime.sh status

docker-cache-prune: ## Reclaim rebuildable Docker BuildKit cache (keeps images, containers, and volumes)
	@docker info >/dev/null 2>&1 || (echo "The Docker daemon is not ready; start it before pruning the build cache."; exit 1)
	docker builder prune --all --force

warroom: ## macOS/Ubuntu: run Council + War Room Web in the foreground
	$(MAKE) -C council-rs warroom-browser

warroom-tauri: ## Open the War Room native desktop shell (Tauri)
	$(MAKE) -C council-rs warroom-dev

warroom-tauri-build: ## Package the War Room native desktop shell (Tauri)
	$(MAKE) -C council-rs warroom-build

build: ## Build the full Rust workspace in release mode
	cargo build --workspace --release

test: ## Run the full Rust workspace test suite
	cargo test --workspace

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "Targets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ { printf "  %-14s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
