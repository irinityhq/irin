#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=test/lib/council_stub_runtime.sh
source "$SCRIPT_DIR/lib/council_stub_runtime.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

runtime=$(COUNCIL_STUB_RUNTIME=auto CI=true council_stub_runtime)
[ "$runtime" = "compose" ] || fail "CI auto mode selected '$runtime', expected compose"

url=$(council_stub_base_url "$runtime" 8765 host.docker.internal)
[ "$url" = "http://council-stub:8765" ] \
    || fail "compose mode selected '$url', expected Compose DNS upstream"

runtime=$(COUNCIL_STUB_RUNTIME=auto CI=false \
    COUNCIL_STUB_CONTAINER_MARKER=/definitely-not-a-container council_stub_runtime)
[ "$runtime" = "host" ] || fail "native auto mode selected '$runtime', expected host"

runtime=$(COUNCIL_STUB_RUNTIME=host CI=false council_stub_runtime)
[ "$runtime" = "host" ] || fail "explicit host mode selected '$runtime', expected host"

url=$(council_stub_base_url "$runtime" 9876 172.17.0.1)
[ "$url" = "http://172.17.0.1:9876" ] \
    || fail "host mode selected '$url', expected native host upstream"

docker_path="$PATH"
if ! docker compose version >/dev/null 2>&1 \
    && [ -x /Applications/Docker.app/Contents/Resources/bin/docker ]; then
    docker_path="/Applications/Docker.app/Contents/Resources/bin:$docker_path"
fi

compose_config=$(PATH="$docker_path" \
    DEMO_LEDGER_KEY=/tmp/ledger-key \
    DEMO_ATTEST_KEYS=/tmp/attest-keys.json \
    docker compose -f "$SCRIPT_DIR/../docker-compose.yml" \
        -f "$SCRIPT_DIR/../docker-compose.demo.yml" config 2>/dev/null)
stub_config=$(printf '%s\n' "$compose_config" | awk '
    /^  council-stub:$/ { capture=1 }
    capture && /^  [a-zA-Z0-9_-]+:$/ && $1 != "council-stub:" { exit }
    capture { print }
')
[ -n "$stub_config" ] || fail "demo Compose config has no council-stub service"
expected_image='python:3.13.5-alpine3.22@sha256:37b14db89f587f9eaa890e4a442a3fe55db452b69cca1403cc730bd0fbdc8aaf'
printf '%s\n' "$stub_config" | grep -Fq "image: $expected_image" \
    || fail "council-stub does not use a digest-pinned lightweight Python image"
printf '%s\n' "$stub_config" | grep -Fq '/app/council_stub.py' \
    || fail "council-stub does not mount the committed stub source"
if printf '%s\n' "$stub_config" | grep -Eq '^[[:space:]]+ports:'; then
    fail "council-stub must not publish a host port"
fi
printf '%s\n' "$compose_config" | grep -Fq 'COUNCIL_BASE_URL: http://council-stub:8765' \
    || fail "demo Gateway is not configured with the Compose DNS upstream"

smoke_config=$(PATH="$docker_path" \
    XAI_API_KEY=x OPENAI_API_KEY=x ANTHROPIC_API_KEY=x NVIDIA_API_KEY=x AUTH_PEPPER=x \
    docker compose -f "$SCRIPT_DIR/../docker-compose.yml" \
        -f "$SCRIPT_DIR/../docker-compose.smoke.yml" config 2>/dev/null)
printf '%s\n' "$smoke_config" | grep -Fq "image: $expected_image" \
    || fail "Phase3 overlay has no digest-pinned Council stub"

grep -Fq 'wget -q -O /dev/null "$council_base_url/api/health"' "$SCRIPT_DIR/smoke_phase3.sh" \
    || fail "Phase3 does not probe the selected upstream from inside Gateway"
grep -Fq 'rm -sf council-stub' "$SCRIPT_DIR/smoke_phase3.sh" \
    || fail "Phase3 cleanup is not scoped to the test-only Council stub service"

echo "PASS: Council stub upstream selection"
