#!/usr/bin/env bash

# Stage a linked Git worktree as a normal repository-backed Docker context.
# Docker cannot follow a worktree's `.git` pointer outside the build context,
# but build.rs must see the real index and worktree state to embed provenance.
stage_worktree_docker_context() {
    local src="$1" dst head main_repo deleted_list raw_copy_list copy_list
    dst="$(mktemp -d -t gateway-docker-ctx.XXXXXX)" || return 1
    head="$(git -C "$src" rev-parse HEAD)" || { rm -rf "$dst"; return 1; }
    main_repo="$(cd "$(git -C "$src" rev-parse --git-common-dir)/.." && pwd)" \
        || { rm -rf "$dst"; return 1; }
    git clone --shared "file://${main_repo}" "$dst" >/dev/null 2>&1 \
        || { rm -rf "$dst"; return 1; }
    git -C "$dst" checkout -f "$head" >/dev/null 2>&1 \
        || { rm -rf "$dst"; return 1; }
    command -v rsync >/dev/null 2>&1 \
        || { echo "rsync is required to stage a worktree Docker context" >&2; rm -rf "$dst"; return 1; }

    deleted_list="$(mktemp -t gateway-docker-ctx-deleted.XXXXXX)" \
        || { rm -rf "$dst"; return 1; }
    git -C "$src" ls-files -z --deleted > "$deleted_list" \
        || { rm -f "$deleted_list"; rm -rf "$dst"; return 1; }
    while IFS= read -r -d '' path; do
        rm -f "$dst/$path"
    done < "$deleted_list"
    rm -f "$deleted_list"

    raw_copy_list="$(mktemp -t gateway-docker-ctx-raw-files.XXXXXX)" \
        || { rm -rf "$dst"; return 1; }
    git -C "$src" ls-files -z --cached --modified --others --exclude-standard > "$raw_copy_list" \
        || { rm -f "$raw_copy_list"; rm -rf "$dst"; return 1; }
    copy_list="$(mktemp -t gateway-docker-ctx-files.XXXXXX)" \
        || { rm -f "$raw_copy_list"; rm -rf "$dst"; return 1; }
    while IFS= read -r -d '' path; do
        [ -e "$src/$path" ] || continue
        printf '%s\0' "$path"
    done < "$raw_copy_list" > "$copy_list"
    rm -f "$raw_copy_list"
    rsync -aI --files-from="$copy_list" --from0 "$src"/ "$dst"/ \
        || { rm -f "$copy_list"; rm -rf "$dst"; return 1; }
    rm -f "$copy_list"
    printf '%s\n' "$dst"
}
