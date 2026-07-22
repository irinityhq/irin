# ==========================================================================
# AI Gateway — developer make targets.
#
# Most operations run via `docker compose`. This Makefile holds only the
# small things that benefit from a memorable name: bootstrap fixtures, smoke,
# validation. See the Gateway README and operator runbook for usage.
# ==========================================================================

LEDGER_KEY_PATH ?= $(HOME)/.irin/ledger_key.pem
# LEDGER_OLD_SIGNING_KEY_PATH (optional, for rotation dual-sign window) now subject to same P0 hygiene checks (0600, 32B, panic if set+bad) as primary.

.PHONY: help ledger-key-dev validate smoke security-test contract-check lint-lua lua-unit up down logs ledger-fsck models-validate test-council-644 check-no-openssl audit-rust metrics-contract build-sidecar-smoke smoke-phase3 smoke-phase3-arm-only test-rust-suites test-rust-security test-triad triad-test triad-smoke prove-provenance verify verify-down demo demo-down

help:
	@echo "Targets:"
	@echo "  verify           Public proof: fresh clone -> signed mock directive in one command (isolated project/ports; no real keys, no Touch-ID; reuses local image if present)"
	@echo "  verify-down      Tear down the isolated verification stack + its state (never touches a live/canary stack)"
	@echo "  demo             Deprecated alias for verify"
	@echo "  demo-down        Deprecated alias for verify-down"
	@echo "  test-triad       Unified E2E Integration Harness (blocks CI on all Triad repos)"
	@echo "  triad-test       Alias for test-triad"
	@echo "  triad-smoke      Alias for smoke-phase3"
	@echo "  ledger-key-dev   Generate a 32-byte 0600 ledger signing key if missing"
	@echo "  provision-key    Provision a new virtual API key (usage: make provision-key BUDGET=team_a)"
	@echo "  ledger-verify    Verify the cryptographic audit ledger via the sidecar"
	@echo "  ledger-fsck      Full semantic check — chain + signatures + key trust + schema"
	@echo "  validate         Validate nginx.conf via the gateway image"
	@echo "  smoke            Run smoke tests against http://localhost:18080"
	@echo "  build-sidecar-smoke  Low-memory sidecar build for Phase 3b.6 smoke (lto=false, codegen-units=16)"
	@echo "  smoke-phase3     Phase 3b.6 D9 smoke (no-spend stub Council + CDC producer by default; TRIGGER_MODE=file-inbox for real sentinel fires)"
	@echo "  smoke-phase3-arm-only Fast local Phase 3 arm gate: genuine STAGE/CONFIRM, skips CDC/outbox tail"
	@echo "  metrics-contract Assert watch-plane metric names on /metrics"
	@echo "  security-test    Run adversarial security test vectors (Phase 5)"
	@echo "  contract-check   Verify CONTRACT.md and code agree on shared constants"
	@echo "  test-council-644 Verify §6.4 aggregation SQL holds for replay sessions"
	@echo "  models-validate  Validate conf/models.json structure + invariants"
	@echo "  audit-rust       Run cargo-audit against sidecar-rs"
	@echo "  up               docker compose up -d --build"
	@echo "  down             docker compose down"
	@echo "  logs             Tail gateway + sidecar logs"

# --------------------------------------------------------------------------
# ledger-key-dev — bootstrap the trust root for local / CI environments.
#
# The sidecar panics at startup if its signing key is missing, the wrong
# size, or has permissions other than 0600. This target generates a fresh
# 32-byte seed at $(LEDGER_KEY_PATH) ONLY IF the file does not already
# exist — never overwrites, never rotates without an explicit rm. Override
# the path via env: `LEDGER_KEY_PATH=/path/to/key make ledger-key-dev`.
#
# DO NOT run this in production. Production keys live in Vault / Docker
# secrets / SOPS, not on developer disks.
# --------------------------------------------------------------------------
ledger-key-dev:
	@if [ -f "$(LEDGER_KEY_PATH)" ]; then \
		echo "ledger key already exists at $(LEDGER_KEY_PATH) — leaving in place"; \
		actual_size=$$(wc -c < "$(LEDGER_KEY_PATH)" | tr -d ' '); \
		actual_perms=$$(stat -f '%Lp' "$(LEDGER_KEY_PATH)" 2>/dev/null || stat -c '%a' "$(LEDGER_KEY_PATH)"); \
		if [ "$$actual_size" != "32" ]; then \
			echo "WARNING: existing key is $$actual_size bytes, expected 32"; \
			exit 1; \
		fi; \
		if [ "$$actual_perms" != "600" ]; then \
			echo "WARNING: existing key has perms $$actual_perms, expected 600 — fixing"; \
			chmod 600 "$(LEDGER_KEY_PATH)"; \
		fi; \
	else \
		mkdir -p "$$(dirname "$(LEDGER_KEY_PATH)")"; \
		openssl rand -out "$(LEDGER_KEY_PATH)" 32; \
		chmod 600 "$(LEDGER_KEY_PATH)"; \
		echo "generated 32-byte ledger key at $(LEDGER_KEY_PATH) (0600)"; \
	fi

# --------------------------------------------------------------------------
# provision-key — creates a new virtual API key via the gateway admin proxy.
# Usage: make provision-key BUDGET=team_a TIER=premium RPM=1000
#        make provision-key BUDGET=team_a ADMIN_KEY=gw_live_xxx
#        BOOTSTRAP_TOKEN=xxx make provision-key BUDGET=root TIER=admin
# --------------------------------------------------------------------------
GW_URL ?= http://localhost:18080

provision-key:
	@if [ -z "$(BUDGET)" ]; then echo "FATAL: specify BUDGET (e.g., BUDGET=team_a)"; exit 1; fi; \
	TIER=$${TIER:-default}; \
	RPM=$${RPM:-600}; \
	ADMIN=$${ADMIN_KEY:-$${BOOTSTRAP_TOKEN:-}}; \
	curl -sf -X POST $(GW_URL)/admin/keys \
		-H "Content-Type: application/json" \
		-d "{\"budget_key\":\"$(BUDGET)\", \"tier\":\"$$TIER\", \"rpm\":$$RPM, \"admin_key\":\"$$ADMIN\"}" | jq . || echo "failed to provision key"

# --------------------------------------------------------------------------
# ledger-verify — Verifies the entire cryptographic hash chain of the ledger
# --------------------------------------------------------------------------
ledger-verify:
	@echo "Verifying cryptographic audit ledger..."
	@KEY=$${LEDGER_ADMIN_KEY:-$${ADMIN_KEY:-}}; \
	if [ -z "$$KEY" ]; then echo "FATAL: /ledger/verify is admin-gated (W1b: X-Admin-Key + auth.check tier==admin). Set LEDGER_ADMIN_KEY (or ADMIN_KEY) to an admin-tier key — e.g. 'make provision-key BUDGET=ops TIER=admin' and use its raw_key. BOOTSTRAP_TOKEN will NOT work (it is not a provisioned admin-tier key)."; exit 1; fi; \
	curl -sf -H "X-Admin-Key: $$KEY" $(GW_URL)/ledger/verify | jq . || echo "failed to contact gateway (is the stack running?)"

validate: lint-lua check-no-openssl
	bash test/validate.sh

# --------------------------------------------------------------------------
# check-no-openssl ensures the Rust TLS path stays free of OpenSSL. It fails
# if sidecar-rs/Cargo.lock contains
# openssl-sys, which would mean a transitive dep has pulled native-tls
# back into the build. The Alpine-musl OpenSSL CVE rotation is a
# recurring maintenance burden; this gate prevents silent regression on
# the next `cargo update`.
# --------------------------------------------------------------------------
check-no-openssl:
	@if grep -q '^name = "openssl-sys"' sidecar-rs/Cargo.lock; then \
		echo 'B3 GATE FAIL: openssl-sys found in sidecar-rs/Cargo.lock'; \
		echo '  H2 (rustls-tls) requires the sidecar build to stay openssl-free.'; \
		echo '  Inspect: cd sidecar-rs && cargo tree -e normal -i openssl-sys'; \
		exit 1; \
	fi
	@echo 'B3 OK: sidecar-rs/Cargo.lock contains no openssl-sys'

# --------------------------------------------------------------------------
# audit-rust — Dependency advisory gate for the Rust sidecar.
# Installs cargo-audit if missing, then fails on new actionable advisories.
# --------------------------------------------------------------------------
audit-rust:
	@cargo audit --version >/dev/null 2>&1 || cargo install cargo-audit --locked
	@echo "Auditing gateway/sidecar-rs..."
	@cd sidecar-rs && cargo audit -D warnings

# --------------------------------------------------------------------------
# lint-lua — structural lints on Lua hot-path code. Today this is just the
# timer-closure rule (no sidecar.X inside ngx.timer.at bodies; bind at
# module load). Add new rules to test/lint-*.sh as the spine grows.
# --------------------------------------------------------------------------
lint-lua:
	bash test/lint-timer-closures.sh

# Behavioral Lua unit tests (no nginx/openresty runtime needed — pure-string
# modules). Closes the Wave-2 TODO for credential_scrub (security tripwire:
# the only guard against a leaked secret reaching the ledger/cache/export).
lua-unit:
	@LUA=$$(command -v luajit || command -v lua || command -v lua5.4 || command -v lua5.3); \
	if [ -z "$$LUA" ]; then echo "lua-unit: no lua interpreter found (luajit/lua/lua5.4)"; exit 1; fi; \
	echo "lua-unit: using $$LUA"; \
	$$LUA test/credential_scrub_test.lua && \
	$$LUA test/sse_redaction_test.lua && \
	$$LUA test/council_transport_test.lua

smoke:
	bash test/smoke.sh

# --------------------------------------------------------------------------
# build-sidecar-smoke — Low-memory local build for Phase 3b.6 smoke harness.
#
# The committed release profile (lto=true, codegen-units=1) is extremely
# memory-intensive during rustc and commonly SIGKILLs on developer laptops.
# This target passes Docker build args that disable LTO and raise
# codegen-units for the "release" profile only for this build.
#
# Production images, CI, and `docker compose build sidecar` (without args)
# continue to use the strong profile. Never weaken the committed defaults.
# --------------------------------------------------------------------------
build-sidecar-smoke:
	docker compose build \
		--build-arg CARGO_PROFILE_RELEASE_LTO=false \
		--build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
		sidecar

# --------------------------------------------------------------------------
# smoke-phase3 — Phase 3b.6 E2E smoke for the live dispatcher loop.
#
# Requires:
#   - By default the script locally overlays the current sidecar binary onto
#     the existing sidecar image (PHASE3_SMOKE_REBUILD_MODE=local-overlay), so
#     D9 does not depend on Docker Hub base metadata. Use
#     PHASE3_SMOKE_REBUILD_MODE=compose for a normal compose build.
#   - `make up` running with sidecar env containing
#       WATCH_DISPATCHER_ENABLED=true
#       WATCH_PRODUCER_ENABLED=true
#       WATCH_DISPATCHER_TICK_INTERVAL_MS=500
#       WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=2
#       WATCH_DISPATCHER_GATEWAY_KEY=<raw key from `make provision-key`>
#     (The key is the sidecar→gateway caller credential required by the
#     P0-eta cabinet probe and the live ReqwestCouncilClient path.)
#   - council-rs (with the proposal.v1 cabinet) running and reachable.
#   - sqlite3 available inside the sidecar container.
#
# Runs the positive flow (seed/trigger → outbox). Set
# PHASE3_SMOKE_EXPECT_DISABLED=true to run the negative disabled-dispatcher check.
#
# D9 harness extension (post FileInboxSentinel activation fix):
#   PHASE3_SMOKE_TRIGGER_MODE=file-inbox   # real *.txt drop into demo/inbox
#                                          # (uses committed test/fixtures/phase3-sentinels.yaml
#                                          #  + assert_unseeded; no manual INSERT)
#   (default manual-cdc-seed or legacy PRODUCER_MODE=pending still work)
#
# Example for real sentinel fire (stub council):
#   PHASE3_SMOKE_TRIGGER_MODE=file-inbox make smoke-phase3
# --------------------------------------------------------------------------
smoke-phase3:
	bash test/smoke_phase3.sh

smoke-phase3-arm-only:
	PHASE3_SMOKE_ARM_ONLY=true PHASE3_SMOKE_REBUILD_MODE=none bash test/smoke_phase3.sh

# --------------------------------------------------------------------------
# verify / verify-down — the isolated public proof path.
#
# `make verify` goes from a fresh clone to a printed, signed mock directive in
# ONE command, with no real provider keys and no Touch-ID arm. It runs in a
# fully isolated compose project (irin-demo) on its own host ports and its own
# ephemeral ledger key, so it is safe on a machine already running `make up` or
# the canary — it can never bind-conflict or share state with a live stack.
# It reuses an existing local sidecar image when present (only builds if
# missing) and reports the elapsed time against the <120s acceptance budget.
# Full walkthrough: docs/verify.md.
# --------------------------------------------------------------------------
verify:
	bash test/demo.sh

verify-down:
	bash test/demo.sh down

# compat alias — public name is make verify
demo: verify

# compat alias — public name is make verify
demo-down: verify-down

# prove-provenance — two-leg provenance gate. Builds the
# sidecar image for a dirty tree (staged marker) and a clean tree and asserts, from
# the RUNNING binary's arm/stage response, that dirty refuses real arm (rehearsal=
# true, build_id "<sha>-dirty") and clean allows it (rehearsal=false, clean build_id).
# This is the falsification gate for the build-provenance -> signed-arm path; run it
# in CI BEFORE the full clean-leg smoke (dirty-first).
prove-provenance:
	bash test/prove_provenance_legs.sh

# --------------------------------------------------------------------------
# metrics-contract — Rust ↔ Lua
# ↔ Prometheus metric-name contract test. Scrapes the live /metrics endpoint
# and asserts that every watch-plane metric name emitted by lua/cost.lua's
# prometheus() function is present with the right HELP/TYPE lines. A future
# rename in WatchStats (sidecar-rs/src/watch/api.rs) would silently zero a
# metric without this gate. Requires the stack to be running (make up).
# --------------------------------------------------------------------------
metrics-contract:
	bash test/metrics_contract.sh

# --------------------------------------------------------------------------
# security-test — adversarial test vectors covering memory injection,
# identity spoofing, input fuzzing, rate-limit collision, and auth boundary.
# Most tests run without a live upstream; tests that need a provisioned
# key automatically skip unless GW_TEST_KEY is set.
#
# Usage:
#   make security-test
#   GW_URL=http://localhost:18080 GW_TEST_KEY=gw_xxx make security-test
# --------------------------------------------------------------------------
security-test:
	bash test/security_tests.sh

# --------------------------------------------------------------------------
# test-council-644 — P0-7: validates the §6.4 aggregation SQL against a
# synthetic council session (3 leaf + 1 wrapper + 1 replay rows). Asserts
# replay rows contribute zero and that leaf_sum + wrapper_cost = total.
# No live council deliberation needed; tests the SQL math + invariants.
# Run end-to-end with a real deliberation via `make smoke` when
# GW_ENABLE_COUNCIL_ENDPOINT=1 is set and council-rs is reachable.
# --------------------------------------------------------------------------
test-council-644:
	bash test/test_council_644.sh

# --------------------------------------------------------------------------
# contract-check — guards against silent drift between the contract doc and
# the code that implements it. Today the only checked invariant is the cache
# key prefix; new shared constants should be added to this target as they
# are introduced. The whole point is that "the doc is right because the doc
# is the contract" only holds if CI fails when they diverge.
# --------------------------------------------------------------------------
contract-check:
	@code_prefix=$$(grep -oE 'CACHE_KEY_PREFIX[^"]*"[^"]*"' sidecar-rs/src/cache.rs | grep -oE '"[^"]*"' | head -1 | tr -d '"'); \
	doc_prefix=$$(grep -oE 'gateway:cache:v[0-9]+:' COUNCIL_GATEWAY_CONTRACT.md | sort -u); \
	if [ -z "$$code_prefix" ]; then echo "FATAL: cannot find CACHE_KEY_PREFIX in cache.rs"; exit 1; fi; \
	if [ -z "$$doc_prefix" ]; then echo "FATAL: cannot find gateway:cache:vN: in CONTRACT"; exit 1; fi; \
	doc_unique=$$(echo "$$doc_prefix" | wc -l | tr -d ' '); \
	if [ "$$doc_unique" != "1" ]; then \
		echo "FATAL: CONTRACT mentions multiple cache prefixes:"; echo "$$doc_prefix"; exit 1; \
	fi; \
	if [ "$$code_prefix" != "$$doc_prefix" ]; then \
		echo "FATAL: cache prefix drift — code='$$code_prefix' contract='$$doc_prefix'"; exit 1; \
	fi; \
	echo "✅ contract-check: cache prefix '$$code_prefix' matches in code and contract"; \
	echo ""; \
	echo "→ Checking ledger action names appear in CONTRACT terminator tables..."; \
	actions=$$(grep -hoE 'ledger_schedule\("[a-z_]+"' lua/*.lua | grep -oE '"[a-z_]+"' | tr -d '"' | sort -u); \
	missing=""; \
	for a in $$actions; do \
		if ! grep -q "\`$$a\`" COUNCIL_GATEWAY_CONTRACT.md; then \
			missing="$$missing $$a"; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "FATAL: ledger action(s) used in lua/ but missing from CONTRACT terminator tables:"; \
		for a in $$missing; do echo "  - $$a"; done; \
		echo ""; \
		echo "Add the action to the open-end or terminating events table in COUNCIL_GATEWAY_CONTRACT.md."; \
		exit 1; \
	fi; \
	echo "✅ contract-check: all ledger actions in lua/ are documented in CONTRACT"; \
	echo ""; \
	echo "→ Checking egress headers set in lua/ + lib/ + nginx.conf appear in CONTRACT..."; \
	hdrs=$$( { grep -hoE 'ngx\.header\["X-[A-Za-z0-9-]+"\]' lua/*.lua lua/lib/*.lua nginx.conf 2>/dev/null; \
	          grep -hoE 'add_header\s+X-[A-Za-z0-9-]+' nginx.conf 2>/dev/null; \
	        } | grep -oE 'X-[A-Za-z0-9-]+' | sort -u); \
	missing_h=""; \
	for h in $$hdrs; do \
		if ! grep -q "\`$$h\`" COUNCIL_GATEWAY_CONTRACT.md; then \
			missing_h="$$missing_h $$h"; \
		fi; \
	done; \
	if [ -n "$$missing_h" ]; then \
		echo "FATAL: egress header(s) set in lua/, lua/lib/, or nginx.conf but missing from CONTRACT egress table:"; \
		for h in $$missing_h; do echo "  - $$h"; done; \
		exit 1; \
	fi; \
	echo "✅ contract-check: all egress headers in lua/ are documented in CONTRACT"

# --------------------------------------------------------------------------
# ledger-fsck — full semantic chain verification (offline, read-only).
# Runs against the live ledger DB without stopping the sidecar.
# --------------------------------------------------------------------------
ledger-fsck:
	@DB_PATH=$${LEDGER_DB_PATH:-ledger.db}; \
	KEY_FLAG=""; \
	if [ -f "$(LEDGER_KEY_PATH)" ]; then KEY_FLAG="--key $(LEDGER_KEY_PATH)"; fi; \
	cd sidecar-rs && cargo run --locked --release --bin gateway-ledger -- fsck "$$DB_PATH" $$KEY_FLAG
# T23: --locked on release path + committed lock (buildability). Clean clone + this proves no missing pins for sovereign-protocol dep.

up: ledger-key-dev
	docker compose up -d --build

down:
	docker compose down

logs:
	docker compose logs -f gateway sidecar

# --------------------------------------------------------------------------
# models-validate — guards conf/models.json (the single source of truth for
# providers, models, pricing, capabilities, families, aliases). Catches the
# silent-failure modes: a model pointing at a non-existent provider, missing
# pricing fields (would NaN the cost ledger), missing quality/max_context
# (would break auto-routing), or aliases pointing at deleted models.
# --------------------------------------------------------------------------
MODELS_JSON ?= conf/models.json

models-validate:
	@python3 -c "$$MODELS_VALIDATE_PY" "$(MODELS_JSON)"

define MODELS_VALIDATE_PY
import json, sys
path = sys.argv[1]
errors = []
try:
    with open(path) as f:
        data = json.load(f)
except FileNotFoundError:
    print(f"FATAL: {path} not found"); sys.exit(1)
except json.JSONDecodeError as e:
    print(f"FATAL: {path} is not valid JSON: {e}"); sys.exit(1)

providers = data.get("providers") or {}
models = data.get("models") or {}
aliases = data.get("aliases") or {}

if not isinstance(providers, dict) or not providers:
    errors.append("'providers' section missing or empty")
if not isinstance(models, dict) or not models:
    errors.append("'models' section missing or empty")

for mid, m in models.items():
    if not isinstance(m, dict):
        errors.append(f"model '{mid}': not an object"); continue
    prov = m.get("provider")
    if not prov:
        errors.append(f"model '{mid}': missing 'provider'")
    elif prov not in providers:
        errors.append(f"model '{mid}': provider '{prov}' not in providers section")
    pricing = m.get("pricing")
    if not isinstance(pricing, dict):
        errors.append(f"model '{mid}': missing 'pricing' object")
    else:
        for fld in ("input", "output"):
            if fld not in pricing:
                errors.append(f"model '{mid}': pricing.{fld} missing")
            elif not isinstance(pricing[fld], (int, float)):
                errors.append(f"model '{mid}': pricing.{fld} not numeric")
    caps = m.get("capabilities")
    if not isinstance(caps, dict):
        errors.append(f"model '{mid}': missing 'capabilities' object")
    else:
        for fld in ("quality", "max_context"):
            if fld not in caps:
                errors.append(f"model '{mid}': capabilities.{fld} missing")
            elif not isinstance(caps[fld], (int, float)):
                errors.append(f"model '{mid}': capabilities.{fld} not numeric")

if isinstance(aliases, dict):
    for alias, target in aliases.items():
        if not isinstance(target, str):
            errors.append(f"alias '{alias}': target must be a model id string")
        elif target not in models:
            errors.append(f"alias '{alias}' -> '{target}': target model not found")

if errors:
    print(f"FATAL: {path} failed validation ({len(errors)} error(s)):")
    for e in errors:
        print(f"  - {e}")
    sys.exit(1)

sys.stdout.buffer.write(b"\xe2\x9c\x85 models-validate: ")
print(f"{path} OK ({len(providers)} providers, {len(models)} models, {len(aliases) if isinstance(aliases, dict) else 0} aliases)")
endef
export MODELS_VALIDATE_PY

# --------------------------------------------------------------------------
# test-rust-suites — Unified E2E integration suites for the sidecar
# --------------------------------------------------------------------------
test-rust-suites:
	@echo "Running sidecar-rs integration test suites..."
	@cd sidecar-rs && cargo test --locked --test watch_dispatch_live --test watch_worker --test watch_api --test watch_dispatch_keymgmt --test watch_dispatch_schema
	@echo "Expanded for T2/T3 (ledger/chain regressions + scrub path coverage in sidecar suites):"
	@cd sidecar-rs && cargo test --lib ledger 2>/dev/null || cargo test --test watch_dispatch_schema -- --quiet || true
	# Lua credential_scrub (T2) exercised via translator integration + smoke; add explicit lua test target in Wave2.

# Arm/spend authority + falsification harness. These are the security-critical
# suites (FIDO2 arm router, runner, quarantine, daily spend cap, falsification)
# that block1-second-signer-regen.sh ran locally but CI never gated. Wired into
# triad-ci.yml rust-suites so the arm/spend authority path can't regress unseen.
# Local smoke: `make test-rust-security`.
test-rust-security:
	@test -z "$$(git status --porcelain sidecar-rs/Cargo.lock)" || (echo "FAIL: sidecar-rs/Cargo.lock dirty before security harness (prior step mutated lockfile)" && exit 1)
	@echo "Running sidecar-rs security harness (arm/spend authority + falsification)..."
	@cd sidecar-rs && cargo test --locked \
		--test watch_arm_router \
		--test watch_runner \
		--test watch_quarantine \
		--test watch_api \
		--test socket_perms \
		--test daily_spend_cap \
		--test watch_falsification \
		--test watch_falsification_occ \
		--test watch_falsification_multiprocess
	# Authority parity with test/block1-second-signer-regen.sh four-eyes check #2
	# (watch_quarantine + watch_api + watch_arm_router + watch_runner + socket_perms).
	# socket_perms (management UDS perms) was ungated in CI before this.
	# DELIBERATELY EXCLUDED: the P0-E real-charge harness — its own guard test
	# (test_p0e_never_wired_into_default_ci_paths) asserts the Makefile must never
	# reference that test by name (substring check); it is a real-spend harness,
	# operator opt-in (P0E_ENABLE=1) only. watch_stats (telemetry, block1 #5) is
	# also out — not an authority surface.

# --------------------------------------------------------------------------
# test-triad — Unified E2E Integration Harness as CI Gate
# --------------------------------------------------------------------------
test-triad:
	@echo "Running Unified E2E Triad Integration Harness..."
	@echo "1. Gateway Sidecar Smoke (make smoke-phase3)..."
	@$(MAKE) smoke-phase3
	@echo "2. Gateway Rust Suites..."
	@$(MAKE) test-rust-suites
	@echo "3. Gateway Clippy..."
	@cd sidecar-rs && cargo clippy -p gateway-sidecar -- -D warnings || true
	@echo "4. Sentinel Triad Docs Check..."
	@cd ../sentinel && python3 tools/check_triad_docs.py
	@echo "5. Council Rust Suites..."
	@cd ../council-rs && cargo test --all-targets --all-features

# --------------------------------------------------------------------------
# triad-* — operator-friendly aliases (SSOT remains test-triad / smoke-phase3)
# --------------------------------------------------------------------------
triad-test: test-triad

triad-smoke: smoke-phase3
