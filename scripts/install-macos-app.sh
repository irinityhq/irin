#!/usr/bin/env bash
# Build, verify, and atomically install the local IRIN app.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$(uname -s)" == "Darwin" ]] || {
  printf 'ERROR: app-install is available only on macOS\n' >&2
  exit 1
}

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

for command in make ditto codesign osascript open pgrep awk sed; do
  command -v "$command" >/dev/null 2>&1 \
    || die "missing macOS app-install command: $command"
done

make -C "$ROOT/council-rs" warroom-build

source_app="${IRIN_APP_SOURCE:-$ROOT/council-rs/warroom-tauri/src-tauri/target/release/bundle/macos/IRIN.app}"
dest_app="${IRIN_APP_DESTINATION:-/Applications/IRIN.app}"
app_binary_name="${IRIN_APP_BINARY_NAME:-council-warroom-tauri}"
source_binary="$source_app/Contents/MacOS/$app_binary_name"
dest_binary="$dest_app/Contents/MacOS/$app_binary_name"
[[ -d "$source_app" ]] || {
  printf 'ERROR: app bundle not found: %s\n' "$source_app" >&2
  exit 1
}
[[ -x "$source_binary" ]] \
  || die "app bundle executable not found: $source_binary"

validate_integer_range() {
  local name="$1"
  local value="$2"
  local minimum="$3"
  local maximum="$4"
  [[ "$value" =~ ^[0-9]+$ ]] \
    && awk -v value="$value" -v minimum="$minimum" -v maximum="$maximum" \
      'BEGIN { exit !(value >= minimum && value <= maximum) }' \
    || die "$name must be an integer between $minimum and $maximum"
}

validate_delay() {
  local name="$1"
  local value="$2"
  local maximum="$3"
  [[ "$value" =~ ^([0-9]+([.][0-9]+)?|[.][0-9]+)$ ]] \
    && awk -v value="$value" -v maximum="$maximum" \
      'BEGIN { exit !(value >= 0 && value <= maximum) }' \
    || die "$name must be a number between 0 and $maximum"
}

open_attempts="${IRIN_APP_OPEN_ATTEMPTS:-5}"
open_retry_delay="${IRIN_APP_OPEN_RETRY_DELAY:-1}"
quit_checks="${IRIN_APP_QUIT_CHECKS:-20}"
quit_check_delay="${IRIN_APP_QUIT_CHECK_DELAY:-0.25}"
process_checks="${IRIN_APP_PROCESS_CHECKS:-8}"
process_check_delay="${IRIN_APP_PROCESS_CHECK_DELAY:-0.5}"
validate_integer_range IRIN_APP_OPEN_ATTEMPTS "$open_attempts" 1 20
validate_integer_range IRIN_APP_QUIT_CHECKS "$quit_checks" 1 120
validate_integer_range IRIN_APP_PROCESS_CHECKS "$process_checks" 2 120
validate_delay IRIN_APP_OPEN_RETRY_DELAY "$open_retry_delay" 60
validate_delay IRIN_APP_QUIT_CHECK_DELAY "$quit_check_delay" 60
validate_delay IRIN_APP_PROCESS_CHECK_DELAY "$process_check_delay" 60

# pgrep treats its pattern as an extended regular expression. Quote every ERE
# metacharacter so paths such as "Applications+[QA]" remain literal paths.
dest_binary_pattern="$(printf '%s\n' "$dest_binary" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"

app_process_running() {
  pgrep -f -x "$dest_binary_pattern" >/dev/null 2>&1
}

parent="$(dirname "$dest_app")"
stage="$parent/.IRIN.app.new.$$"
backup="$parent/.IRIN.app.old.$$"

cleanup() {
  local status=$?
  trap - EXIT
  rm -rf -- "$stage"
  if [[ -e "$backup" && ! -e "$dest_app" ]]; then
    mv "$backup" "$dest_app" || {
      printf 'ERROR: failed to restore previous app from %s\n' "$backup" >&2
    }
  fi
  exit "$status"
}
trap cleanup EXIT

ditto "$source_app" "$stage"
codesign --verify --deep --strict "$stage"
osascript -e 'tell application "IRIN" to quit' >/dev/null 2>&1 || true

# Replacing a bundle while its executable still runs can leave Launch Services
# pointing at the retired inode. Wait boundedly for the exact installed bundle
# process to leave before moving either application.
for ((check = 1; check <= quit_checks; check++)); do
  if ! app_process_running; then
    break
  fi
  if (( check == quit_checks )); then
    die "IRIN did not exit before app replacement"
  fi
  sleep "$quit_check_delay"
done

if [[ -e "$dest_app" ]]; then
  mv "$dest_app" "$backup"
fi
if ! mv "$stage" "$dest_app"; then
  exit 1
fi
rm -rf -- "$backup"
trap - EXIT
printf 'Installed %s\n' "$dest_app"

launched=0
for ((attempt = 1; attempt <= open_attempts; attempt++)); do
  if open "$dest_app"; then
    consecutive_process_checks=0
    for ((check = 1; check <= process_checks; check++)); do
      if app_process_running; then
        consecutive_process_checks=$((consecutive_process_checks + 1))
        if (( consecutive_process_checks >= 2 )); then
          launched=1
          break 2
        fi
      else
        consecutive_process_checks=0
      fi
      (( check == process_checks )) || sleep "$process_check_delay"
    done
  fi
  (( attempt == open_attempts )) || sleep "$open_retry_delay"
done

if [[ "$launched" != 1 ]]; then
  printf "ERROR: installed app but automatic launch failed. Run exactly: open '%s'\n" \
    "$dest_app" >&2
  exit 1
fi
printf 'Launched %s\n' "$dest_app"
