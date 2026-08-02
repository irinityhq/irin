#!/usr/bin/env bash
# link-ship-board.sh — durable ship-board install into an IRIN worktree.
#
# W3 durability decision (operator-owned source, not a sanitized tracked copy):
#   SSOT:  ${IRIN_SHIP_BOARD_HOME:-$HOME/.local/share/irin/ship-board}
#   Link:  <worktree>/tools/ship-board → that directory
#
# IRIN_ROOT is bound per invocation from the worktree that owns the symlink
# (run.sh resolves via the logical path). This script never writes a shared
# global .irin-root that would retarget every board instance.
#
# Usage:
#   scripts/link-ship-board.sh                 # link into this checkout
#   scripts/link-ship-board.sh --status
#   scripts/link-ship-board.sh --bootstrap-from PATH
#   scripts/link-ship-board.sh --migrate-legacy
#       If tools/ship-board is a real directory, move it aside and link.
#   scripts/link-ship-board.sh --all-worktrees
#       Link every worktree of this monorepo (skips already-linked).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOME_DIR="${IRIN_SHIP_BOARD_HOME:-$HOME/.local/share/irin/ship-board}"
LINK_PATH="$ROOT/tools/ship-board"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*"; }

MODE="link"
BOOTSTRAP_FROM=""
TARGET_ROOT="$ROOT"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --status) MODE="status"; shift ;;
    --migrate-legacy) MODE="migrate"; shift ;;
    --all-worktrees) MODE="all"; shift ;;
    --worktree)
      TARGET_ROOT="${2:-}"
      [[ -n "$TARGET_ROOT" && -d "$TARGET_ROOT" ]] || die "--worktree requires an existing directory"
      TARGET_ROOT="$(cd "$TARGET_ROOT" && pwd)"
      shift 2
      ;;
    --bootstrap-from)
      BOOTSTRAP_FROM="${2:-}"
      MODE="bootstrap"
      shift 2
      ;;
    -h|--help)
      sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done
# Operations target TARGET_ROOT (default: checkout that owns this script).
ROOT="$TARGET_ROOT"
LINK_PATH="$ROOT/tools/ship-board"

status_report() {
  note "IRIN_SHIP_BOARD_HOME=$HOME_DIR"
  note "worktree_root=$ROOT"
  note "worktree_link=$LINK_PATH"
  if [[ -d "$HOME_DIR" ]]; then
    note "durable_home: present"
  else
    note "durable_home: ABSENT"
  fi
  # Shared pin must not exist (would retarget every instance).
  if [[ -f "$HOME_DIR/.irin-root" ]]; then
    note "WARNING: $HOME_DIR/.irin-root still present (stale global pin); remove it"
  fi
  if [[ -L "$LINK_PATH" ]]; then
    note "worktree_link: symlink → $(readlink "$LINK_PATH")"
  elif [[ -d "$LINK_PATH" ]]; then
    note "worktree_link: real directory (legacy; run --migrate-legacy)"
  elif [[ -e "$LINK_PATH" ]]; then
    note "worktree_link: unexpected non-directory"
  else
    note "worktree_link: ABSENT"
  fi
}

bootstrap_home() {
  local src="$1"
  [[ -d "$src" ]] || die "bootstrap source missing: $src"
  if [[ -d "$HOME_DIR" ]] && [[ -n "$(ls -A "$HOME_DIR" 2>/dev/null || true)" ]]; then
    die "durable home already non-empty: $HOME_DIR (refusing overwrite)"
  fi
  mkdir -p "$(dirname "$HOME_DIR")"
  rsync -a \
    --exclude node_modules \
    --exclude dist \
    --exclude .DS_Store \
    --exclude '*.log' \
    --exclude .irin-root \
    "$src"/ "$HOME_DIR"/
  # Never keep a global root pin in the durable home.
  rm -f "$HOME_DIR/.irin-root"
  note "bootstrapped durable ship-board → $HOME_DIR"
}

link_one() {
  local wt="$1"
  local link="$wt/tools/ship-board"
  mkdir -p "$wt/tools"
  if [[ -L "$link" ]]; then
    local current
    current="$(readlink "$link")"
    if [[ "$current" == "$HOME_DIR" ]]; then
      note "already linked: $link → $HOME_DIR"
      return 0
    fi
    note "ERROR: tools/ship-board is a symlink to $current (expected $HOME_DIR) in $wt"
    return 1
  fi
  if [[ -e "$link" ]]; then
    note "ERROR: tools/ship-board is a real path in $wt — use --migrate-legacy --worktree $wt"
    return 1
  fi
  ln -s "$HOME_DIR" "$link"
  # Per-worktree marker only (optional diagnostic); never a shared global pin.
  printf '%s\n' "$wt" >"$wt/.irin-board-linked"
  note "linked $link → $HOME_DIR (IRIN_ROOT bound at run time from this worktree)"
  return 0
}

migrate_legacy() {
  [[ -d "$HOME_DIR" ]] || die "durable home missing; bootstrap first"
  if [[ -L "$LINK_PATH" ]]; then
    note "already a symlink; nothing to migrate"
    # Drop stale global pin if present.
    rm -f "$HOME_DIR/.irin-root"
    return 0
  fi
  [[ -d "$LINK_PATH" ]] || die "no legacy real directory at $LINK_PATH"
  local bak
  bak="${LINK_PATH}.legacy-$(date -u +%Y%m%dT%H%M%SZ)"
  note "moving legacy board aside → $bak"
  mv "$LINK_PATH" "$bak"
  ln -s "$HOME_DIR" "$LINK_PATH"
  rm -f "$HOME_DIR/.irin-root"
  printf '%s\n' "$ROOT" >"$ROOT/.irin-board-linked"
  note "migrated: $LINK_PATH → $HOME_DIR"
  note "legacy copy retained at $bak (delete when satisfied)"
}

if [[ "$MODE" == "status" ]]; then
  status_report
  exit 0
fi

if [[ "$MODE" == "bootstrap" ]]; then
  [[ -n "$BOOTSTRAP_FROM" ]] || die "--bootstrap-from requires PATH"
  bootstrap_home "$BOOTSTRAP_FROM"
  exit 0
fi

[[ -d "$HOME_DIR" ]] || die "durable ship-board missing at $HOME_DIR (run --bootstrap-from PATH first)"
# Scrub any stale global pin that would retarget every instance.
if [[ -f "$HOME_DIR/.irin-root" ]]; then
  note "removing stale global $HOME_DIR/.irin-root (per-worktree IRIN_ROOT only)"
  rm -f "$HOME_DIR/.irin-root"
fi

if [[ "$MODE" == "migrate" ]]; then
  migrate_legacy
  exit 0
fi

if [[ "$MODE" == "all" ]]; then
  # Discover worktrees from any IRIN checkout (this script's original tree).
  discover_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  while IFS= read -r line; do
    case "$line" in
      worktree\ *)
        wt="${line#worktree }"
        [[ -d "$wt" ]] || continue
        note "--- $wt ---"
        link="$wt/tools/ship-board"
        if [[ -d "$link" && ! -L "$link" ]]; then
          note "legacy real directory at $link — migrating"
          bak="${link}.legacy-$(date -u +%Y%m%dT%H%M%SZ)"
          mv "$link" "$bak"
          note "retained legacy copy: $bak"
        fi
        if ! link_one "$wt"; then
          note "SKIP $wt"
        fi
        ;;
    esac
  done < <(git -C "$discover_root" worktree list --porcelain 2>/dev/null)
  exit 0
fi

# Default: link this worktree only.
link_one "$ROOT"
