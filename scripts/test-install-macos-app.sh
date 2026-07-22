#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAKE_BIN="$TMP/bin"
SOURCE_APP="$TMP/build+[src]/Council (Build).app"
DEST_APP="$TMP/Applications+[QA]/Council (War).app"
ACTION_LOG="$TMP/actions.log"
APP_BINARY_NAME="council.warroom+tauri"
APP_BINARY_REL="Contents/MacOS/$APP_BINARY_NAME"
EXPECTED_BINARY="$DEST_APP/$APP_BINARY_REL"
EXPECTED_BINARY_PATTERN="$(printf '%s\n' "$EXPECTED_BINARY" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
FALSE_POSITIVE_BINARY="${EXPECTED_BINARY/council.warroom+tauri/councilXwarroomYtauri}"
mkdir -p "$FAKE_BIN" "$SOURCE_APP/Contents/MacOS" "$(dirname "$DEST_APP")"
: >"$ACTION_LOG"
printf 'new app\n' >"$SOURCE_APP/Contents/version"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$SOURCE_APP/$APP_BINARY_REL"
chmod +x "$SOURCE_APP/$APP_BINARY_REL"

write_fake() {
  local name="$1"
  shift
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    printf '%s\n' "$@"
  } >"$FAKE_BIN/$name"
  chmod +x "$FAKE_BIN/$name"
}

write_fake uname 'printf '\''Darwin\n'\'''
write_fake make 'printf '\''make %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"'
write_fake ditto \
  'printf '\''ditto %s -> %s\n'\'' "$1" "$2" >>"${FAKE_ACTION_LOG:?}"' \
  '/bin/cp -R "$1" "$2"'
write_fake codesign 'printf '\''codesign %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"'
write_fake osascript 'printf '\''osascript %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"'
write_fake open \
  'count=0; [[ ! -f "${FAKE_OPEN_COUNT_FILE:?}" ]] || count="$(<"$FAKE_OPEN_COUNT_FILE")"' \
  'count=$((count + 1)); printf '\''%s\n'\'' "$count" >"$FAKE_OPEN_COUNT_FILE"' \
  'printf '\''open %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"' \
  '[[ "${FAKE_OPEN_FAIL:-0}" != 1 ]] || exit 1' \
  '(( count > ${FAKE_OPEN_FAILS:-0} ))'
write_fake pgrep \
  '[[ "$1" == "-f" && "$2" == "-x" && "$3" == "${FAKE_EXPECTED_BINARY_PATTERN:?}" ]] || exit 64' \
  '[[ "${FAKE_EXPECTED_BINARY:?}" =~ $3 ]] || exit 65' \
  '[[ ! "${FAKE_FALSE_POSITIVE_BINARY:?}" =~ $3 ]] || exit 66' \
  'if /usr/bin/grep -Fq "open ${FAKE_DEST_APP:?}" "${FAKE_ACTION_LOG:?}"; then' \
  '  open_count="$(<"${FAKE_OPEN_COUNT_FILE:?}")"' \
  '  printf '\''pgrep launch open=%s %s\n'\'' "$open_count" "$3" >>"$FAKE_ACTION_LOG"' \
  '  [[ "${FAKE_PROCESS_NEVER:-0}" != 1 ]] || exit 1' \
  '  (( open_count > ${FAKE_PROCESS_MISSING_OPEN_SUCCESSES:-0} ))' \
  'else' \
  '  count=0; [[ ! -f "${FAKE_QUIT_COUNT_FILE:?}" ]] || count="$(<"$FAKE_QUIT_COUNT_FILE")"' \
  '  count=$((count + 1)); printf '\''%s\n'\'' "$count" >"$FAKE_QUIT_COUNT_FILE"' \
  '  if (( count <= ${FAKE_OLD_PROCESS_HITS:-0} )); then' \
  '    printf '\''pgrep quit running %s\n'\'' "$3" >>"$FAKE_ACTION_LOG"' \
  '    exit 0' \
  '  fi' \
  '  printf '\''pgrep quit stopped %s\n'\'' "$3" >>"$FAKE_ACTION_LOG"' \
  '  exit 1' \
  'fi'
write_fake mv \
  'printf '\''mv %s -> %s\n'\'' "$1" "$2" >>"${FAKE_ACTION_LOG:?}"' \
  'if [[ "${FAKE_MV_FAIL_STAGE:-0}" == 1 && "$1" == *.new.* ]]; then exit 73; fi' \
  '/bin/mv "$@"'

run_install() {
  rm -f "$TMP/pgrep-quit-count"
  HOME="$TMP/home" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_APP_SOURCE="$SOURCE_APP" \
  IRIN_APP_DESTINATION="$DEST_APP" \
  IRIN_APP_BINARY_NAME="$APP_BINARY_NAME" \
  IRIN_APP_OPEN_ATTEMPTS="${IRIN_APP_OPEN_ATTEMPTS:-5}" \
  IRIN_APP_OPEN_RETRY_DELAY="${IRIN_APP_OPEN_RETRY_DELAY:-0}" \
  IRIN_APP_QUIT_CHECKS="${IRIN_APP_QUIT_CHECKS:-4}" \
  IRIN_APP_QUIT_CHECK_DELAY="${IRIN_APP_QUIT_CHECK_DELAY:-0}" \
  IRIN_APP_PROCESS_CHECKS="${IRIN_APP_PROCESS_CHECKS:-3}" \
  IRIN_APP_PROCESS_CHECK_DELAY="${IRIN_APP_PROCESS_CHECK_DELAY:-0}" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_OPEN_COUNT_FILE="$TMP/open-count" \
  FAKE_QUIT_COUNT_FILE="$TMP/pgrep-quit-count" \
  FAKE_EXPECTED_BINARY="$EXPECTED_BINARY" \
  FAKE_EXPECTED_BINARY_PATTERN="$EXPECTED_BINARY_PATTERN" \
  FAKE_FALSE_POSITIVE_BINARY="$FALSE_POSITIVE_BINARY" \
  FAKE_DEST_APP="$DEST_APP" \
  /usr/bin/make -s -C "$ROOT" app-install
}

# The successful public command builds, atomically installs, and launches the
# installed bundle through the macOS open command.
INSTALL_OUTPUT="$(run_install 2>&1)"
[[ "$(<"$DEST_APP/Contents/version")" == 'new app' ]]
grep -Fq "make -C $ROOT/council-rs warroom-build" "$ACTION_LOG"
grep -Fq "open $DEST_APP" "$ACTION_LOG"
[[ "$(grep -Fc "pgrep launch open=1 $EXPECTED_BINARY_PATTERN" "$ACTION_LOG")" == 2 ]]
grep -Fq "Installed $DEST_APP" <<<"$INSTALL_OUTPUT"
grep -Fq "Launched $DEST_APP" <<<"$INSTALL_OUTPUT"
printf 'app-install build, atomic install, and launch: PASS\n'

# Launch Services can briefly reject a replaced running bundle. Retry that
# bounded transient rather than failing an otherwise successful installation.
rm -f "$TMP/open-count"
: >"$ACTION_LOG"
RETRY_OUTPUT="$(FAKE_OPEN_FAILS=2 run_install 2>&1)"
[[ "$(<"$TMP/open-count")" == 3 ]]
grep -Fq "Launched $DEST_APP" <<<"$RETRY_OUTPUT"
printf 'app-install retries transient Launch Services failure: PASS\n'

# `open` can report success before the new bundle process exists. Retry the
# launch until the exact installed executable persists across two checks.
rm -f "$TMP/open-count"
: >"$ACTION_LOG"
PROCESS_RETRY_OUTPUT="$(FAKE_PROCESS_MISSING_OPEN_SUCCESSES=2 run_install 2>&1)"
[[ "$(<"$TMP/open-count")" == 3 ]]
[[ "$(grep -Fc "pgrep launch open=3 $EXPECTED_BINARY_PATTERN" "$ACTION_LOG")" == 2 ]]
grep -Fq "Launched $DEST_APP" <<<"$PROCESS_RETRY_OUTPUT"
printf 'app-install retries open success without a persistent process: PASS\n'

# A running old executable must leave before the installed bundle is moved.
rm -rf "$DEST_APP"
mkdir -p "$DEST_APP/Contents/MacOS"
printf 'previous app\n' >"$DEST_APP/Contents/version"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$DEST_APP/$APP_BINARY_REL"
chmod +x "$DEST_APP/$APP_BINARY_REL"
rm -f "$TMP/open-count"
: >"$ACTION_LOG"
QUIT_OUTPUT="$(FAKE_OLD_PROCESS_HITS=2 run_install 2>&1)"
[[ "$(grep -Fc "pgrep quit running $EXPECTED_BINARY_PATTERN" "$ACTION_LOG")" == 2 ]]
quit_line="$(grep -nF "pgrep quit stopped $EXPECTED_BINARY_PATTERN" "$ACTION_LOG" | cut -d: -f1)"
replace_line="$(grep -nF "mv $DEST_APP -> " "$ACTION_LOG" | cut -d: -f1)"
[[ -n "$quit_line" && -n "$replace_line" && "$quit_line" -lt "$replace_line" ]]
grep -Fq "Launched $DEST_APP" <<<"$QUIT_OUTPUT"
printf 'app-install waits for old process before replacement: PASS\n'

# The quit wait is bounded: a process that never leaves fails before either
# installed bundle is moved, preserving the old application in place.
rm -rf "$DEST_APP"
mkdir -p "$DEST_APP/Contents/MacOS"
printf 'previous app\n' >"$DEST_APP/Contents/version"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$DEST_APP/$APP_BINARY_REL"
chmod +x "$DEST_APP/$APP_BINARY_REL"
rm -f "$TMP/open-count"
: >"$ACTION_LOG"
set +e
QUIT_TIMEOUT_OUTPUT="$(FAKE_OLD_PROCESS_HITS=99 run_install 2>&1)"
QUIT_TIMEOUT_STATUS=$?
set -e
[[ "$QUIT_TIMEOUT_STATUS" -ne 0 ]]
grep -Fq "did not exit before app replacement" <<<"$QUIT_TIMEOUT_OUTPUT"
! grep -Fq "mv $DEST_APP -> " "$ACTION_LOG"
[[ "$(<"$DEST_APP/Contents/version")" == 'previous app' ]]
printf 'app-install quit timeout preserves old app before replacement: PASS\n'

# Every launch-control setting is validated before staging or stopping the old
# app. An invalid attempt count must leave the installed bundle untouched.
rm -rf "$DEST_APP"
mkdir -p "$DEST_APP/Contents/MacOS"
printf 'previous app\n' >"$DEST_APP/Contents/version"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' >"$DEST_APP/$APP_BINARY_REL"
chmod +x "$DEST_APP/$APP_BINARY_REL"
: >"$ACTION_LOG"
set +e
INVALID_ATTEMPTS_OUTPUT="$(IRIN_APP_OPEN_ATTEMPTS=0 run_install 2>&1)"
INVALID_ATTEMPTS_STATUS=$?
set -e
[[ "$INVALID_ATTEMPTS_STATUS" -ne 0 ]]
grep -Fq 'IRIN_APP_OPEN_ATTEMPTS must be an integer between 1 and 20' \
  <<<"$INVALID_ATTEMPTS_OUTPUT"
! grep -Eq '^(ditto|codesign|osascript|pgrep|mv|open) ' "$ACTION_LOG"
[[ "$(<"$DEST_APP/Contents/version")" == 'previous app' ]]
printf 'app-install invalid attempts fail before staging or replacement: PASS\n'

# Invalid delays fail at the same boundary, before the quit request or any
# filesystem replacement can affect the existing app.
: >"$ACTION_LOG"
set +e
INVALID_DELAY_OUTPUT="$(IRIN_APP_PROCESS_CHECK_DELAY=-1 run_install 2>&1)"
INVALID_DELAY_STATUS=$?
set -e
[[ "$INVALID_DELAY_STATUS" -ne 0 ]]
grep -Fq 'IRIN_APP_PROCESS_CHECK_DELAY must be a number between 0 and 60' \
  <<<"$INVALID_DELAY_OUTPUT"
! grep -Eq '^(ditto|codesign|osascript|pgrep|mv|open) ' "$ACTION_LOG"
[[ "$(<"$DEST_APP/Contents/version")" == 'previous app' ]]
printf 'app-install invalid delay fails before staging or replacement: PASS\n'

# A failed stage-to-destination rename restores the previous application.
rm -rf "$DEST_APP"
mkdir -p "$DEST_APP/Contents"
printf 'previous app\n' >"$DEST_APP/Contents/version"
: >"$ACTION_LOG"
rm -f "$TMP/open-count"
set +e
FAILED_OUTPUT="$(FAKE_MV_FAIL_STAGE=1 run_install 2>&1)"
FAILED_STATUS=$?
set -e
[[ "$FAILED_STATUS" -ne 0 ]] || {
  printf 'expected atomic install failure\n%s\n' "$FAILED_OUTPUT" >&2
  exit 1
}
[[ "$(<"$DEST_APP/Contents/version")" == 'previous app' ]] || {
  printf 'failed install did not restore previous app\n%s\n' "$FAILED_OUTPUT" >&2
  exit 1
}
printf 'app-install failure restores previous app: PASS\n'

# If Launch Services cannot open the installed bundle, fail with the exact
# manual command while leaving the newly installed app in place.
: >"$ACTION_LOG"
set +e
OPEN_OUTPUT="$(FAKE_OPEN_FAIL=1 run_install 2>&1)"
OPEN_STATUS=$?
set -e
[[ "$OPEN_STATUS" -ne 0 ]] || {
  printf 'expected launch failure to fail app-install\n%s\n' "$OPEN_OUTPUT" >&2
  exit 1
}
grep -Fq "Run exactly: open '$DEST_APP'" <<<"$OPEN_OUTPUT"
[[ "$(<"$DEST_APP/Contents/version")" == 'new app' ]]
printf 'app-install exact manual launch fallback: PASS\n'

# Launch Services success without the exact persistent bundle process is still
# a failed launch and retains the same exact manual fallback command.
: >"$ACTION_LOG"
rm -f "$TMP/open-count"
set +e
PROCESS_OUTPUT="$(FAKE_PROCESS_NEVER=1 run_install 2>&1)"
PROCESS_STATUS=$?
set -e
[[ "$PROCESS_STATUS" -ne 0 ]] || {
  printf 'expected missing app process to fail app-install\n%s\n' "$PROCESS_OUTPUT" >&2
  exit 1
}
[[ "$(<"$TMP/open-count")" == 5 ]]
grep -Fq "Run exactly: open '$DEST_APP'" <<<"$PROCESS_OUTPUT"
printf 'app-install rejects open success without persistent process: PASS\n'
