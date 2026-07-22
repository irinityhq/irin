//! Embed the build identity into the binary at
//! compile time so the arm-confirm challenge can bind WHICH code is being
//! armed. The identity is the git SHA plus an optional `-dirty` suffix.
//! It is captured once here via `std::process::Command`;
//! `attest::build_id()` reads the embedded `env!()` constants — NEVER live git
//! at runtime (confirm re-reads the embedded constant, Q1).
//!
//! std-only by design (no external build deps). A git failure (no repo /
//! detached / git absent) degrades to a stable `"unknown"` SHA + `dirty=true`
//! so an unidentifiable build can only ever arm as DARK/rehearsal (B6).

use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run when HEAD moves or the index changes so the embedded SHA/dirty
    // flag never goes stale across incremental builds.
    //
    // HEAD/index alone miss a plain `git commit`
    // ON A BRANCH — that advances `.git/refs/heads/<branch>` (or rewrites
    // `.git/packed-refs` after a pack), not `.git/HEAD`, so the embedded SHA went
    // STALE across commits on the same branch. Watch the ref store too. The
    // `../.git/...` variants cover the sidecar-as-subdir layout; a linked
    // worktree's `.git` is a FILE pointing at the real gitdir, so its HEAD lives
    // under the worktree gitdir (covered by `.git/HEAD`) while branch refs still
    // resolve through the common dir's refs/heads + packed-refs (covered below).
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=../.git/refs/heads");
    println!("cargo:rerun-if-changed=../.git/packed-refs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    emit_tracked_file_rerun_paths();

    let sha = git_sha().unwrap_or_else(|| "unknown".to_string());
    // Fail-closed: if we cannot determine cleanliness, treat as dirty.
    let dirty = git_is_dirty().unwrap_or(true);

    println!("cargo:rustc-env=GW_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=GW_BUILD_DIRTY={dirty}");
}

fn emit_tracked_file_rerun_paths() {
    let Ok(root_output) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    else {
        return;
    };
    if !root_output.status.success() {
        return;
    }
    let root = String::from_utf8_lossy(&root_output.stdout)
        .trim()
        .to_string();
    if root.is_empty() {
        return;
    }

    let Ok(files_output) = Command::new("git")
        .args(["-C", &root, "ls-files", "-z"])
        .output()
    else {
        return;
    };
    if !files_output.status.success() {
        return;
    }
    for relative in files_output.stdout.split(|byte| *byte == 0) {
        if !relative.is_empty() {
            println!(
                "cargo:rerun-if-changed={}",
                Path::new(&root)
                    .join(String::from_utf8_lossy(relative).as_ref())
                    .display()
            );
        }
    }
}

fn git_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn git_is_dirty() -> Option<bool> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Any non-empty porcelain output ⇒ tracked working tree / index changes.
    Some(!out.stdout.iter().all(u8::is_ascii_whitespace))
}
