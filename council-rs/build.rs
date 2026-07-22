//! Embed the source identity in the Council binary at compile time.
//!
//! Runtime health must describe the code that was compiled, never the checkout
//! that happens to be present when the process is queried.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    emit_git_rerun_paths(&manifest_dir);
    emit_tracked_file_rerun_paths(&manifest_dir);

    // Packaging / throwaway builds: honor explicit provenance so host + council match.
    // Prefer COUNCIL_BUILD_GIT_SHA, then IRIN_TAURI_BUILD_GIT_SHA, then git, then SOURCE_SHA.txt.
    println!("cargo:rerun-if-env-changed=COUNCIL_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=COUNCIL_BUILD_DIRTY");
    println!("cargo:rerun-if-env-changed=IRIN_TAURI_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=IRIN_TAURI_BUILD_DIRTY");

    let sha = env_nonempty("COUNCIL_BUILD_GIT_SHA")
        .or_else(|| env_nonempty("IRIN_TAURI_BUILD_GIT_SHA"))
        .or_else(|| git_output(&manifest_dir, &["rev-parse", "HEAD"]))
        .or_else(read_source_sha_file)
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = env_nonempty("COUNCIL_BUILD_DIRTY")
        .or_else(|| env_nonempty("IRIN_TAURI_BUILD_DIRTY"))
        .map(|s| s == "true" || s == "1")
        .unwrap_or_else(|| git_is_dirty(&manifest_dir).unwrap_or(true));

    println!("cargo:rustc-env=COUNCIL_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=COUNCIL_BUILD_DIRTY={dirty}");
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_source_sha_file() -> Option<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // council-rs → src → throwaway root
    for rel in ["../SOURCE_SHA.txt", "../../SOURCE_SHA.txt", "../../../SOURCE_SHA.txt"] {
        let path = manifest.join(rel);
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let v = raw.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn git_is_dirty(dir: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.iter().all(u8::is_ascii_whitespace))
}

fn emit_tracked_file_rerun_paths(dir: &Path) {
    let Some(root) = git_output(dir, &["rev-parse", "--show-toplevel"]) else {
        return;
    };
    let Ok(output) = Command::new("git")
        .args(["-C", &root, "ls-files", "-z"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for relative in output.stdout.split(|byte| *byte == 0) {
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

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn emit_git_rerun_paths(dir: &Path) {
    for git_dir_arg in ["--git-dir", "--git-common-dir"] {
        let Some(raw) = git_output(dir, &["rev-parse", git_dir_arg]) else {
            continue;
        };
        let git_dir = if Path::new(&raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            dir.join(raw)
        };
        for path in ["HEAD", "index", "packed-refs", "refs/heads"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(path).display());
        }
    }
}
