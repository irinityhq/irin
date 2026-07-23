#!/usr/bin/env bash
# Install pinned, checksum-verified actionlint into ignored repo-local state.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}
version=1.7.12
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)
    platform=darwin_arm64
    archive_sha=aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f
    binary_sha=8db11704dc296f096216db4db65d86cd7f0ebfdf4c38453a1da276b137b88388
    ;;
  Darwin/x86_64)
    platform=darwin_amd64
    archive_sha=5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644
    binary_sha=d1f7cee75ae2873609bd9567b4600bebc5315a5e733e73202987a44fafdd53b2
    ;;
  Linux/x86_64)
    platform=linux_amd64
    archive_sha=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
    binary_sha=c872d6db8c6bf83a8eaa704fc93999f027d55dffbc63b8a6abdccb47df5f4cd4
    ;;
  Linux/aarch64|Linux/arm64)
    platform=linux_arm64
    archive_sha=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6
    binary_sha=ac0323433c2853ec3fb978c611430c5b3dc5d43c58d1a1ec031b00ab572beb60
    ;;
  *) printf 'ERROR: unsupported actionlint platform: %s/%s\n' "$os" "$arch" >&2; exit 1 ;;
esac

file_sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    printf 'ERROR: missing SHA-256 command (shasum or sha256sum)\n' >&2
    return 1
  fi
}

destination="$ROOT/.irin-tools/bin/actionlint"
if [[ -x "$destination" ]] && [[ "$(file_sha256 "$destination")" == "$binary_sha" ]]; then
  printf 'actionlint %s: ready (%s)\n' "$version" "$destination"
  exit 0
fi

for command in curl tar; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'ERROR: missing tool-bootstrap command: %s\n' "$command" >&2
    exit 1
  }
done

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-actionlint.XXXXXX")"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM
archive="$tmp/actionlint.tar.gz"
url="https://github.com/rhysd/actionlint/releases/download/v$version/actionlint_${version}_${platform}.tar.gz"
printf 'Downloading actionlint %s for %s\n' "$version" "$platform"
curl -fsSL --retry 3 --retry-all-errors -o "$archive" "$url"
actual="$(file_sha256 "$archive")"
[[ "$actual" == "$archive_sha" ]] || {
  printf 'ERROR: actionlint archive checksum mismatch\n' >&2
  exit 1
}
tar -xzf "$archive" -C "$tmp" actionlint
[[ "$(file_sha256 "$tmp/actionlint")" == "$binary_sha" ]] || {
  printf 'ERROR: actionlint executable checksum mismatch\n' >&2
  exit 1
}
mkdir -p "$(dirname "$destination")"
install -m 0755 "$tmp/actionlint" "$destination"
"$destination" -version
