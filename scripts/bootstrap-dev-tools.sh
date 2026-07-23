#!/usr/bin/env bash
# Install pinned, checksum-verified ship tooling into ignored repo-local state.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}
version=0.19.9
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)
    triple=aarch64-apple-darwin
    archive_sha=be6fd555e910ac360e25cbef4f16ead47d87d2545fa07aa27223ef0f9af1a02c
    binary_sha=26335000fbf0698b4eb646ffeb6fca02a9cb12f5b9f461170ffa384d7b6ab1a4
    ;;
  Darwin/x86_64)
    triple=x86_64-apple-darwin
    archive_sha=3336724665a3aef124a9e4c79cb59968df36d21bfcda5ae596abe2a7874b1938
    binary_sha=31d4ebbd9cc37903d478af142ab153b930e0e3ec679eec53b87c2128dde71dff
    ;;
  Linux/x86_64)
    triple=x86_64-unknown-linux-musl
    archive_sha=f1f8eedc2a3ac297c540873f93785d4104b102c0079506b2a6b3221b7ec956af
    binary_sha=df554c960ac6e6db83047e6d06e1451dcb48201553299935ffdf4216d413e6e3
    ;;
  Linux/aarch64|Linux/arm64)
    triple=aarch64-unknown-linux-musl
    archive_sha=32580dcc2bc13fbeeb7c50edc38ef99c3b9ce9569a1d07a96e11b3b941f3e72e
    binary_sha=3b45687215d00900a88bc9f0142ac9ba9fa1e636d02ab62f7d7d62dfdf5ac788
    ;;
  *) printf 'ERROR: unsupported cargo-deny platform: %s/%s\n' "$os" "$arch" >&2; exit 1 ;;
esac

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    printf 'ERROR: missing SHA-256 command (shasum or sha256sum)\n' >&2
    return 1
  fi
}

destination="$ROOT/.irin-tools/bin/cargo-deny"
if [[ -x "$destination" ]]; then
  cached_sha="$(sha256_file "$destination")"
  if [[ "$cached_sha" == "$binary_sha" ]] &&
    "$destination" --version | grep -Fxq "cargo-deny $version"; then
    printf 'cargo-deny %s: checksum verified (%s)\n' "$version" "$destination"
    exit 0
  fi
  printf 'cargo-deny cache: rejected (checksum or version mismatch); reinstalling\n' >&2
fi

for command in curl tar; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'ERROR: missing tool-bootstrap command: %s\n' "$command" >&2
    exit 1
  }
done

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-tools.XXXXXX")"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM
archive="$tmp/cargo-deny.tar.gz"
url="https://github.com/EmbarkStudios/cargo-deny/releases/download/$version/cargo-deny-$version-$triple.tar.gz"
printf 'Downloading cargo-deny %s for %s\n' "$version" "$triple"
curl -fsSL --retry 3 --retry-all-errors -o "$archive" "$url"
actual="$(sha256_file "$archive")"
[[ "$actual" == "$archive_sha" ]] || {
  printf 'ERROR: cargo-deny archive checksum mismatch\n' >&2
  exit 1
}
tar -xzf "$archive" -C "$tmp"
candidate="$(find "$tmp" -type f -name cargo-deny -print -quit)"
[[ -n "$candidate" ]] || {
  printf 'ERROR: cargo-deny missing from verified archive\n' >&2
  exit 1
}
[[ "$(sha256_file "$candidate")" == "$binary_sha" ]] || {
  printf 'ERROR: cargo-deny executable checksum mismatch\n' >&2
  exit 1
}
mkdir -p "$(dirname "$destination")"
install -m 0755 "$candidate" "$destination"
[[ "$(sha256_file "$destination")" == "$binary_sha" ]] || {
  printf 'ERROR: installed cargo-deny executable checksum mismatch\n' >&2
  exit 1
}
"$destination" --version | grep -Fx "cargo-deny $version"
