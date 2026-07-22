#!/bin/sh

irin_gateway_env_path() {
  printf '%s\n' "${IRIN_GATEWAY_ENV:-${XDG_CONFIG_HOME:-$HOME/.config}/irin/gateway.env}"
}

irin_env_value() {
  key=$1
  file=$2
  value=$(awk -v key="$key" 'index($0, key "=") == 1 { sub(/^[^=]*=/, ""); print; exit }' "$file")
  case "$value" in
    \"*\") value=${value#\"}; value=${value%\"} ;;
    \'*\') value=${value#\'}; value=${value%\'} ;;
  esac
  printf '%s\n' "$value"
}

irin_arm_principal_token() {
  env_file=$(irin_gateway_env_path)
  if [ ! -r "$env_file" ]; then
    echo "gateway environment is not readable: $env_file" >&2
    return 1
  fi
  principals=$(irin_env_value GW_ARM_PRINCIPALS "$env_file")
  token=$(printf '%s\n' "$principals" | tr ',' '\n' | awk -F: '$1 == "sovereign-op" { sub(/^[^:]*:/, ""); print; exit }')
  if [ -z "$token" ]; then
    echo "sovereign-op token not found in $env_file" >&2
    return 1
  fi
  case "$token" in
    *[!A-Za-z0-9_.-]*)
      echo "sovereign-op token has characters outside [A-Za-z0-9_.-]" >&2
      return 1
      ;;
  esac
  printf '%s\n' "$token"
}

irin_compose_project() {
  printf '%s\n' "${IRIN_COMPOSE_PROJECT:-gateway}"
}

irin_sidecar_socket_volume() {
  printf '%s_sidecar_sock\n' "$(irin_compose_project)"
}

irin_sidecar_data_volume() {
  printf '%s_sidecar_data\n' "$(irin_compose_project)"
}
