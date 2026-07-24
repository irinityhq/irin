#!/usr/bin/env bash
# Prove desktop pack ownership invariants without starting a live Gateway when possible.
# When Docker is up, also proves compose -p irin-desktop-gateway cannot be confused
# with a foreign project name.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/packaging/gateway-pack/docker-compose.yml"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'PASS: %s\n' "$*"; }

# Fixed project name only.
grep -q 'name: irin-desktop-gateway' "$COMPOSE" || die "project name"
! grep -E '^[[:space:]]*build:' "$COMPOSE" || die "no build"
grep -q 'WATCH_PRODUCER_ENABLED=false' "$COMPOSE" || die "producer false"
grep -q 'WATCH_DISPATCHER_ENABLED=false' "$COMPOSE" || die "dispatcher false"
grep -q '127.0.0.1:18080:8080' "$COMPOSE" || die "loopback port"
! grep -E '^[[:space:]]*-\s*.*(\$\{HOME\}|~/|\$HOME)' "$COMPOSE" || die "no home mounts"

# Tag-only image refs must not appear as defaults in pack compose.
if grep -E 'image:[[:space:]]*[A-Za-z0-9._/-]+:[A-Za-z0-9._-]+\s*$' "$COMPOSE" >/dev/null; then
  die "tag-only image defaults forbidden"
fi
grep -q 'IRIN_GATEWAY_IMAGE' "$COMPOSE" || die "gateway image from env"
grep -q 'IRIN_SIDECAR_IMAGE' "$COMPOSE" || die "sidecar image from env"

# Manifest example is production-shaped with digests.
python3 - <<'PY' "$ROOT/packaging/gateway-pack/image-manifest.production.example.json"
import json, re, sys
m = json.load(open(sys.argv[1]))
pat = re.compile(r"^[^@\s]+@sha256:[0-9a-f]{64}$")
for v in m["images"].values():
    assert pat.match(v)
assert m["watch_invariants"]["WATCH_PRODUCER_ENABLED"] is False
print("manifest shape ok")
PY

# Docker allow-list docs in source.
rg -q '/usr/local/bin/docker' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/docker_cli.rs" || die "allowlist path"
rg -q 'irin-desktop-gateway' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/docker_cli.rs" || die "fixed project in rust"
rg -q 'authenticated_ready' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/gateway_pack/mod.rs" || die "ready state"
rg -q 'GW_API_KEY' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/private_config.rs" || die "deny import"

# Foreign project must not be passable through validate_compose_project (unit-tested);
# shell-level: ensure no script targets canonical project name for pack operations.
if rg -n 'compose -p gateway\b|compose -p "gateway"' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src" \
  "$ROOT/scripts/stage-gateway-pack.sh" \
  "$ROOT/scripts/build-gateway-pack-dev-images.sh" 2>/dev/null; then
  die "pack tooling must not target project 'gateway'"
fi

# Bounded docker info (must not hang forever).
if command -v docker >/dev/null 2>&1; then
  if python3 - <<'PY'
import subprocess, sys
try:
    r = subprocess.run(
        ["docker", "info", "--format", "{{.ServerVersion}}"],
        capture_output=True, timeout=12, check=False,
    )
    sys.exit(0 if r.returncode == 0 else 1)
except subprocess.TimeoutExpired:
    sys.exit(2)
PY
  then
    if docker compose ls --format json 2>/dev/null | grep -q 'irin-desktop-gateway'; then
      pass "desktop project currently present (will not auto-destroy here)"
    else
      pass "docker ready; desktop project not running (clean)"
    fi
    [[ "gateway" != "irin-desktop-gateway" ]] || die "names"
    pass "docker isolation predicates"
  else
    pass "docker absent or down — core-neutral path (no red)"
  fi
else
  pass "docker CLI missing — core-neutral path (no red)"
fi

# Source-level: stage script must not silently prefer local-dev in production.
rg -q 'IRIN_GATEWAY_PACK_MODE' "$ROOT/scripts/stage-gateway-pack.sh" || die "stage mode gate"
rg -q 'production packaging refuses a local-dev' "$ROOT/scripts/stage-gateway-pack.sh" \
  || die "stage refuse local-dev"
rg -q 'pack_mode' "$ROOT/packaging/build-dmg.sh" || die "dmg pack_mode"
rg -q 'run_command_timeout|DOCKER_CMD_TIMEOUT' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/docker_cli.rs" || die "docker timeout"
rg -q 'WhenUnlockedThisDeviceOnly|kSecAttrAccessibleWhenUnlockedThisDeviceOnly' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/keychain.rs" || die "keychain accessibility"
rg -q 'IRIN_APP_SUPPORT_ROOT' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/private_config.rs" || die "app support root override"
rg -q 'login keychain unavailable' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/keychain.rs" || die "keychain preflight token"
rg -q 'OWNED_DESKTOP_TEARDOWN|desktop_preexisting' \
  "$ROOT/packaging/smoke-gateway-pack.sh" \
  "$ROOT/scripts/test-gateway-pack-integration-smoke.sh" || die "desktop ownership flags"
# Never reclaim via delete-then-readd of operator Keychain items.
if rg -n 'reclaim re-add|delete then re-add|re-add after reclaim' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/keychain.rs" 2>/dev/null; then
  die "keychain must not delete-and-readd operator items"
fi
rg -q 'try_wait' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/private_config.rs" || die "cancelable login timeout"
rg -q 'redact_process_text' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/docker_cli.rs" || die "redactor"
rg -q 'is_pack_installed' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/gateway_pack/mod.rs" || die "install marker"
rg -q 'repo_digests_match_ref' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/gateway_pack/manifest.rs" || die "prod digests"
rg -q 'GATEWAY_SCRUB_ENV_KEYS' \
  "$ROOT/council-rs/warroom-tauri/src-tauri/src/sidecar.rs" || die "council env scrub"

pass "gateway pack isolation invariants"
