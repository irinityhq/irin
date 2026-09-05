use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn git_is_dirty(dir: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.iter().all(u8::is_ascii_whitespace))
}

pub(crate) fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
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

pub(crate) fn emit_git_rerun_paths(dir: &Path) {
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
