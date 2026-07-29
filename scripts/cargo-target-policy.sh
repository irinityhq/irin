#!/usr/bin/env bash
# Bound and serialize IRIN Cargo output before a build can consume the disk.
set -euo pipefail

usage() {
  printf 'usage: %s link|prepare <checkout> | run <checkout> <command> [args...]\n' "$0" >&2
  exit 2
}

mode="${1:-}"
checkout="${2:-}"
[[ -n "$mode" && -n "$checkout" ]] || usage
shift 2
checkout="$(cd "$checkout" && pwd -P)"

target="${IRIN_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-${HOME}/.cache/irin/cargo-target}}"
[[ "$target" == /* ]] || {
  printf 'ERROR: IRIN Cargo target must be an absolute path: %s\n' "$target" >&2
  exit 1
}
case "$target" in
  /|"${HOME}"|"$checkout")
    printf 'ERROR: refusing unsafe IRIN Cargo target path: %s\n' "$target" >&2
    exit 1
    ;;
esac
mkdir -p "$target"

# The warm workspace settles near 22 GiB without incremental state. Leave a
# narrow compilation margin while refusing builds before the volume is critical.
max_kib="${IRIN_CARGO_TARGET_MAX_KIB:-25165824}"
min_free_kib="${IRIN_CARGO_MIN_FREE_KIB:-10485760}"
[[ "$max_kib" =~ ^[0-9]+$ && "$min_free_kib" =~ ^[0-9]+$ ]] || {
  printf 'ERROR: Cargo target ceiling and free-space floor must be integer KiB values\n' >&2
  exit 1
}

link_one() {
  local link_path="$1"
  if [[ -L "$link_path" ]]; then
    [[ "$(readlink "$link_path")" == "$target" ]] || {
      printf 'ERROR: Cargo target symlink points elsewhere: %s -> %s\n' \
        "$link_path" "$(readlink "$link_path")" >&2
      exit 1
    }
    return 0
  fi
  if [[ -e "$link_path" ]]; then
    printf 'ERROR: private Cargo target already exists at %s; clean this generated directory once, remove it, and retry so IRIN can adopt the bounded shared target\n' \
      "$link_path" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$link_path")"
  ln -s "$target" "$link_path"
}

link_checkout() {
  link_one "$checkout/target"
  link_one "$checkout/council-rs/warroom-tauri/src-tauri/target"
}

prune_incremental() {
  local incremental
  while IFS= read -r incremental; do
    [[ "$incremental" == "$target"/*/incremental ]] || {
      printf 'ERROR: refusing unexpected incremental path: %s\n' "$incremental" >&2
      exit 1
    }
    find "$incremental" -depth -delete
  done < <(find "$target" -type d -name incremental -prune -print)
}

prepare_target() {
  local size_kib available_kib max_mib floor_mib
  link_checkout
  prune_incremental
  size_kib="$(du -sk "$target" | awk '{print $1}')"
  max_mib=$((max_kib / 1024))
  if (( size_kib > max_kib )); then
    printf 'ERROR: shared Cargo target is %s MiB and exceeds the %s MiB ceiling; reclaim the generated target before building\n' \
      "$((size_kib / 1024))" "$max_mib" >&2
    exit 1
  fi
  available_kib="$(df -Pk "$target" | awk 'NR == 2 { print $4 }')"
  floor_mib=$((min_free_kib / 1024))
  if (( available_kib < min_free_kib )); then
    printf 'ERROR: Cargo build blocked with %s MiB free; IRIN requires a %s MiB free-space floor\n' \
      "$((available_kib / 1024))" "$floor_mib" >&2
    exit 1
  fi
  printf 'Cargo target policy: %s MiB cached, %s MiB free, incremental=off\n' \
    "$((size_kib / 1024))" "$((available_kib / 1024))"
}

lock_dir="$target/.irin-build.lock"
acquire_lock() {
  local owner=""
  if [[ "${IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK:-0}" != 1 ]] && \
      { pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; }; then
    printf 'ERROR: another Cargo or rustc process is active; IRIN proof builds are serialized to prevent duplicate disk growth\n' >&2
    exit 1
  fi
  if mkdir "$lock_dir" 2>/dev/null; then
    printf '%s\n' "$$" >"$lock_dir/pid"
    return 0
  fi
  [[ ! -f "$lock_dir/pid" ]] || owner="$(sed -n '1p' "$lock_dir/pid")"
  if [[ "$owner" =~ ^[0-9]+$ ]] && kill -0 "$owner" 2>/dev/null; then
    printf 'ERROR: another IRIN build owns the shared Cargo target (pid %s)\n' "$owner" >&2
    exit 1
  fi
  find "$lock_dir" -depth -delete
  mkdir "$lock_dir"
  printf '%s\n' "$$" >"$lock_dir/pid"
}

release_lock() {
  [[ -d "$lock_dir" ]] || return 0
  find "$lock_dir" -depth -delete
}

case "$mode" in
  link)
    (( $# == 0 )) || usage
    link_checkout
    ;;
  prepare)
    (( $# == 0 )) || usage
    prepare_target
    ;;
  run)
    (( $# > 0 )) || usage
    acquire_lock
    # shellcheck disable=SC2329 # invoked indirectly by trap
    cleanup_lock() { release_lock; }
    trap cleanup_lock EXIT INT TERM
    prepare_target
    set +e
    IRIN_CARGO_POLICY_ACTIVE=1 CARGO_INCREMENTAL=0 "$@"
    status=$?
    prepare_target
    policy_status=$?
    [[ "$status" -ne 0 ]] || status="$policy_status"
    set -e
    release_lock
    trap - EXIT INT TERM
    exit "$status"
    ;;
  *)
    usage
    ;;
esac
