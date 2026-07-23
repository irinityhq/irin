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
  Darwin/arm64) triple=aarch64-apple-darwin; sha=be6fd555e910ac360e25cbef4f16ead47d87d2545fa07aa27223ef0f9af1a02c ;;
  Darwin/x86_64) triple=x86_64-apple-darwin; sha=3336724665a3aef124a9e4c79cb59968df36d21bfcda5ae596abe2a7874b1938 ;;
  Linux/x86_64) triple=x86_64-unknown-linux-musl; sha=f1f8eedc2a3ac297c540873f93785d4104b102c0079506b2a6b3221b7ec956af ;;
  Linux/aarch64|Linux/arm64) triple=aarch64-unknown-linux-musl; sha=32580dcc2bc13fbeeb7c50edc38ef99c3b9ce9569a1d07a96e11b3b941f3e72e ;;
  *) printf 'ERROR: unsupported cargo-deny platform: %s/%s\n' "$os" "$arch" >&2; exit 1 ;;
esac

destination="$ROOT/.irin-tools/bin/cargo-deny"
if [[ -x "$destination" ]] && "$destination" --version | grep -Fq "cargo-deny $version"; then
  printf 'cargo-deny %s: ready (%s)\n' "$version" "$destination"
  exit 0
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
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
else
  actual="$(sha256sum "$archive" | awk '{print $1}')"
fi
[[ "$actual" == "$sha" ]] || {
  printf 'ERROR: cargo-deny archive checksum mismatch\n' >&2
  exit 1
}
tar -xzf "$archive" -C "$tmp"
candidate="$(find "$tmp" -type f -name cargo-deny -print -quit)"
[[ -n "$candidate" ]] || {
  printf 'ERROR: cargo-deny missing from verified archive\n' >&2
  exit 1
}
mkdir -p "$(dirname "$destination")"
install -m 0755 "$candidate" "$destination"
"$destination" --version
