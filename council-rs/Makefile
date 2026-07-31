# Council-RS - developer convenience targets
#
# Keep this minimal. The real authority for Council is the release binary
# built from this tree. These targets exist to make local hygiene cheap.

.DEFAULT_GOAL := help

.PHONY: help
help:
	@echo "Council developer targets (invoke from the IRIN root with: make -C council-rs <target>)"
	@echo "  warroom-dev              Run the Tauri development shell"
	@echo "  warroom-browser          Run Council + browser War Room for development"
	@echo "  warroom-build            Build and locally sign the Tauri bundle"
	@echo "  warroom-export           Build static War Room assets"
	@echo "  warroom-web-check        Run the Linux-safe War Room web/export gate"
	@echo "  warroom-tauri-check      Run Tauri export markers and Rust tests"
	@echo "  warroom-check            Run the complete macOS War Room gate"
	@echo "  warroom-product-check    Exercise the exact embedded export in Chromium"
	@echo "  warroom-native-smoke     Build and prove the native macOS app window"
	@echo "  audit-rust               Audit Council Rust dependencies"
	@echo "  check-no-legacy-reqwest  Reject the retired reqwest/native-TLS stack"

# --------------------------------------------------------------------------
# audit-rust - Runs cargo-audit against the current lockfile.
#
# Installs cargo-audit if missing (idempotent for local use).
# Fails on any new vulnerability or unmaintained crate that is not explicitly allowed.
#
# Run this before any dependency changes and as part of routine hygiene.
# --------------------------------------------------------------------------
audit-rust:
	@cargo audit --version >/dev/null 2>&1 || cargo install cargo-audit --locked
	@echo "Auditing council-rs..."
	@cargo audit -D warnings --ignore RUSTSEC-2024-0436

.PHONY: audit-rust

# --------------------------------------------------------------------------
# check-no-legacy-reqwest - P2 hygiene guard (post Council-RS Reqwest 0.13 wave)
#
# Ensures we never regress to reqwest 0.12 + native-tls + openssl-sys.
# This was the root cause of the transitive island that had to be cleaned
# during the 0.13 wave (via fastembed/hf-hub).
#
# Run this as part of local hygiene and wire it into CI.
# Modeled after Gateway's check-no-openssl (B3).
# --------------------------------------------------------------------------
check-no-legacy-reqwest:
	@echo "Checking for legacy reqwest 0.12 / native-tls / openssl-sys..."
	@if cargo tree --target all -p council-rs --no-dedupe | grep -E '(^|[[:space:]])reqwest v0\.12|(^|[[:space:]])native-tls v|(^|[[:space:]])openssl-sys v'; then \
		echo "FAIL: legacy reqwest/native TLS dependency present in active tree"; \
		exit 1; \
	fi
	@echo "OK: No reqwest v0.12, native-tls, or openssl-sys in the active tree"

.PHONY: check-no-legacy-reqwest

.PHONY: warroom-dev warroom-build warroom-sign warroom-export warroom-web-check warroom-web-check-isolated warroom-tauri-check warroom-check warroom-check-isolated warroom-product-check warroom-native-smoke warroom-browser

# --------------------------------------------------------------------------
# warroom-browser — one command: council :8765 + Next.js :3010 (post-reboot)
# --------------------------------------------------------------------------
warroom-browser:
	bash scripts/warroom-browser-dev.sh

# --------------------------------------------------------------------------
# warroom-dev / warroom-build — IRIN desktop (Tauri, Phase 2 hybrid Next.js)
# --------------------------------------------------------------------------
warroom-dev:
	bash scripts/warroom-tauri-dev.sh

warroom-build:
	cargo build --release -p council-rs --bin council --locked
	bash warroom-tauri/scripts/stage-bundle-inputs.sh
	cd warroom/web && npm ci
	cd warroom-tauri && npm ci && npm run tauri build $(if $(strip $(TAURI_BUILD_CONFIG)),-- --config "$(TAURI_BUILD_CONFIG)",)
	$(MAKE) warroom-sign

warroom-sign:
	@if [ "$$(uname -s)" = "Darwin" ]; then \
	  APP="warroom-tauri/src-tauri/target/release/bundle/macos/IRIN.app"; \
	  test -d "$$APP" || { echo "Missing app bundle: $$APP" >&2; exit 1; }; \
	  if ! codesign --verify --deep --strict "$$APP" >/dev/null 2>&1; then \
	    echo "Ad-hoc signing local IRIN bundle"; \
	    codesign --force --deep --sign - "$$APP"; \
	  fi; \
	  codesign --verify --deep --strict "$$APP"; \
	fi

warroom-export:
	bash warroom-tauri/scripts/build-warroom-assets.sh

warroom-web-check:
	bash ../scripts/with-test-ports.sh $(MAKE) warroom-web-check-isolated

warroom-web-check-isolated:
	cd warroom/web && npm ci
	cd warroom/web && npm audit --omit=dev --audit-level=high
	cd warroom/web && npm run lint
	cd warroom/web && npm run typecheck
	cd warroom/web && npm run test:ci
	$(MAKE) warroom-product-check

warroom-tauri-check:
	WARROOM_SMOKE_SKIP_WEB_LINT=1 bash warroom-tauri/scripts/smoke-hybrid-build.sh

warroom-check:
	bash ../scripts/with-test-ports.sh $(MAKE) warroom-check-isolated

warroom-check-isolated:
	$(MAKE) warroom-web-check-isolated
	$(MAKE) warroom-tauri-check

warroom-product-check:
	cd warroom/web && [ -d node_modules ] || npm ci
	bash warroom-tauri/scripts/build-warroom-assets.sh
	cd warroom/web && npm run test:export

warroom-native-smoke:
	bash ../scripts/smoke-macos-tauri-app.sh
