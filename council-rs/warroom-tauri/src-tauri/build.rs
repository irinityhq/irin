mod build_support;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sha =
        git_output(&manifest_dir, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git_is_dirty(&manifest_dir).unwrap_or(true);

    println!("cargo:rustc-env=IRIN_TAURI_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=IRIN_TAURI_BUILD_DIRTY={dirty}");
    emit_git_rerun_paths(&manifest_dir);
    for path in build_support::tracked_file_rerun_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    tauri_build::build()
}

fn git_is_dirty(dir: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| !output.stdout.iter().all(u8::is_ascii_whitespace))
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
