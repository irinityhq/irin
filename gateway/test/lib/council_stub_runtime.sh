#!/usr/bin/env bash

# Select where the deterministic no-spend Council stub runs. CI and callers
# already running inside a container must use a sibling Compose service: a
# host-side process would otherwise live in the caller's network namespace,
# not the Docker host network reached by host-gateway.
council_stub_runtime() {
    local requested="${COUNCIL_STUB_RUNTIME:-auto}"
    case "$requested" in
        compose|host)
            printf '%s\n' "$requested"
            ;;
        auto)
            if [ "${CI:-false}" = "true" ] \
                || [ -e "${COUNCIL_STUB_CONTAINER_MARKER:-/.dockerenv}" ]; then
                printf '%s\n' compose
            else
                printf '%s\n' host
            fi
            ;;
        *)
            echo "unsupported COUNCIL_STUB_RUNTIME=$requested (expected auto, compose, or host)" >&2
            return 2
            ;;
    esac
}

council_stub_base_url() {
    local runtime="$1" port="$2" host="$3"
    case "$runtime" in
        compose) printf 'http://council-stub:%s\n' "$port" ;;
        host) printf 'http://%s:%s\n' "$host" "$port" ;;
        *)
            echo "unsupported Council stub runtime: $runtime" >&2
            return 2
            ;;
    esac
}
