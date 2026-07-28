#!/usr/bin/env bash
# Install pinned, checksum-verified ship tooling into ignored repo-local state.
# Tools: cargo-deny, opengrep, selene
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}

BIN_DIR="$ROOT/.irin-tools/bin"
mkdir -p "$BIN_DIR"

os="$(uname -s)"
arch="$(uname -m)"

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

require_cmds() {
  local command
  for command in "$@"; do
    command -v "$command" >/dev/null 2>&1 || {
      printf 'ERROR: missing tool-bootstrap command: %s\n' "$command" >&2
      exit 1
    }
  done
}

# ---------------------------------------------------------------------------
# cargo-deny 0.19.9 (tar.gz archive + nested binary checksum)
# ---------------------------------------------------------------------------
install_cargo_deny() {
  local version=0.19.9
  local triple archive_sha binary_sha destination tmp archive url actual candidate

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

  destination="$BIN_DIR/cargo-deny"
  if [[ -x "$destination" ]]; then
    if [[ "$(sha256_file "$destination")" == "$binary_sha" ]] &&
      "$destination" --version | grep -Fxq "cargo-deny $version"; then
      printf 'cargo-deny %s: checksum verified (%s)\n' "$version" "$destination"
      return 0
    fi
    printf 'cargo-deny cache: rejected (checksum or version mismatch); reinstalling\n' >&2
  fi

  require_cmds curl tar
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-tools-deny.XXXXXX")"
  # EXIT (not only RETURN): set -e failures must still clean the temp dir.
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
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
  install -m 0755 "$candidate" "$destination"
  [[ "$(sha256_file "$destination")" == "$binary_sha" ]] || {
    printf 'ERROR: installed cargo-deny executable checksum mismatch\n' >&2
    exit 1
  }
  "$destination" --version | grep -Fx "cargo-deny $version"
  trap - EXIT
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------
# opengrep 1.26.0 (single-file release assets; one checksum per binary)
# ---------------------------------------------------------------------------
install_opengrep() {
  local version=1.26.0
  local asset binary_sha destination tmp candidate url actual

  case "$os/$arch" in
    Darwin/arm64)
      asset=opengrep_osx_arm64
      binary_sha=513ff8491f7254c9a672cf8421136a537eb53b2a8af748568bd697acdc59eefe
      ;;
    Darwin/x86_64)
      asset=opengrep_osx_x86
      binary_sha=36c00a2b6eeb45796275e69cb8f74ef27c42724a1b3c98f6c8d861bad7a8529d
      ;;
    Linux/x86_64)
      asset=opengrep_musllinux_x86
      binary_sha=18aeca114221e2816ec26e1a731f1a2583408c8e4578cd868cd2d47c12fd29f8
      ;;
    Linux/aarch64|Linux/arm64)
      asset=opengrep_musllinux_aarch64
      binary_sha=d4e20ac57b6f9bb32c2b0ffc0501b8c6acb92ecee60f11f1cd72db9b11647857
      ;;
    *) printf 'ERROR: unsupported opengrep platform: %s/%s\n' "$os" "$arch" >&2; exit 1 ;;
  esac

  destination="$BIN_DIR/opengrep"
  if [[ -x "$destination" ]]; then
    if [[ "$(sha256_file "$destination")" == "$binary_sha" ]] &&
      "$destination" --version 2>/dev/null | grep -Eq "(^| )${version}([ .]|$)"; then
      printf 'opengrep %s: checksum verified (%s)\n' "$version" "$destination"
      return 0
    fi
    printf 'opengrep cache: rejected (checksum or version mismatch); reinstalling\n' >&2
  fi

  require_cmds curl
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-tools-opengrep.XXXXXX")"
  # EXIT (not only RETURN): set -e failures must still clean the temp dir.
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  candidate="$tmp/opengrep"
  url="https://github.com/opengrep/opengrep/releases/download/v${version}/${asset}"
  printf 'Downloading opengrep %s (%s)\n' "$version" "$asset"
  curl -fsSL --retry 3 --retry-all-errors -o "$candidate" "$url"
  actual="$(sha256_file "$candidate")"
  [[ "$actual" == "$binary_sha" ]] || {
    printf 'ERROR: opengrep binary checksum mismatch\n' >&2
    printf '  expected: %s\n' "$binary_sha" >&2
    printf '  actual:   %s\n' "$actual" >&2
    exit 1
  }
  chmod 0755 "$candidate"
  install -m 0755 "$candidate" "$destination"
  [[ "$(sha256_file "$destination")" == "$binary_sha" ]] || {
    printf 'ERROR: installed opengrep executable checksum mismatch\n' >&2
    exit 1
  }
  "$destination" --version 2>/dev/null | grep -Eq "(^| )${version}([ .]|$)" || {
    printf 'ERROR: installed opengrep version mismatch (want %s)\n' "$version" >&2
    exit 1
  }
  printf 'opengrep %s: installed (%s)\n' "$version" "$destination"
  trap - EXIT
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------
# selene 0.31.0 (zip archive + nested binary checksum)
# Official assets: Kampfkarren/selene — macos (arm64) + linux (x86_64).
# ---------------------------------------------------------------------------
install_selene() {
  local version=0.31.0
  local asset archive_sha binary_sha destination tmp archive url actual candidate

  case "$os/$arch" in
    Darwin/arm64)
      asset=selene-${version}-macos.zip
      archive_sha=67f644e57e14ccb74a0c272bc44af0dc7909d8bdff58e4e59bb3524717da5741
      binary_sha=bc0457112c121a9f608f6b55857b4ab6843d92f1ce6884d32aa5d3b7000a007b
      ;;
    Linux/x86_64)
      asset=selene-${version}-linux.zip
      archive_sha=dac452422747999ec4919bbb8bb52992b66aae533b60022bf005669de8616671
      binary_sha=30887c8f10ab901fe5883ef655f7b9fe47e628b83c709c3d7548b02e966e67a4
      ;;
    *)
      # Advisory tool: skip where official assets do not exist instead of
      # aborting make tools; the runner treats a missing binary as a skip.
      printf 'selene: skip unsupported platform: %s/%s\n' "$os" "$arch" >&2
      printf '  supported: Darwin/arm64, Linux/x86_64 (official 0.31.0 release assets)\n' >&2
      return 0
      ;;
  esac

  destination="$BIN_DIR/selene"
  if [[ -x "$destination" ]]; then
    if [[ "$(sha256_file "$destination")" == "$binary_sha" ]] &&
      "$destination" --version 2>/dev/null | grep -Eq "(^| )${version}([ .]|$)"; then
      printf 'selene %s: checksum verified (%s)\n' "$version" "$destination"
      return 0
    fi
    printf 'selene cache: rejected (checksum or version mismatch); reinstalling\n' >&2
  fi

  require_cmds curl unzip
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-tools-selene.XXXXXX")"
  # EXIT (not only RETURN): set -e failures must still clean the temp dir.
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  archive="$tmp/selene.zip"
  url="https://github.com/Kampfkarren/selene/releases/download/${version}/${asset}"
  printf 'Downloading selene %s (%s)\n' "$version" "$asset"
  curl -fsSL --retry 3 --retry-all-errors -o "$archive" "$url"
  actual="$(sha256_file "$archive")"
  [[ "$actual" == "$archive_sha" ]] || {
    printf 'ERROR: selene archive checksum mismatch\n' >&2
    printf '  expected: %s\n' "$archive_sha" >&2
    printf '  actual:   %s\n' "$actual" >&2
    exit 1
  }
  unzip -qo "$archive" -d "$tmp"
  candidate="$(find "$tmp" -type f -name selene -print -quit)"
  [[ -n "$candidate" ]] || {
    printf 'ERROR: selene missing from verified archive\n' >&2
    exit 1
  }
  [[ "$(sha256_file "$candidate")" == "$binary_sha" ]] || {
    printf 'ERROR: selene executable checksum mismatch\n' >&2
    exit 1
  }
  install -m 0755 "$candidate" "$destination"
  [[ "$(sha256_file "$destination")" == "$binary_sha" ]] || {
    printf 'ERROR: installed selene executable checksum mismatch\n' >&2
    exit 1
  }
  "$destination" --version 2>/dev/null | grep -Eq "(^| )${version}([ .]|$)" || {
    printf 'ERROR: installed selene version mismatch (want %s)\n' "$version" >&2
    exit 1
  }
  printf 'selene %s: installed (%s)\n' "$version" "$destination"
  trap - EXIT
  rm -rf "$tmp"
}

install_cargo_deny
install_opengrep
install_selene
