mod build_support;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Prefer explicit packaging provenance, then git, then unknown.
    let sha = std::env::var("IRIN_TAURI_BUILD_GIT_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| git_output(&manifest_dir, &["rev-parse", "HEAD"]))
        .or_else(read_source_sha_file)
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::env::var("IRIN_TAURI_BUILD_DIRTY")
        .ok()
        .map(|s| s == "true" || s == "1")
        .unwrap_or_else(|| git_is_dirty(&manifest_dir).unwrap_or(true));

    println!("cargo:rustc-env=IRIN_TAURI_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=IRIN_TAURI_BUILD_DIRTY={dirty}");
    println!("cargo:rerun-if-env-changed=IRIN_TAURI_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=IRIN_TAURI_BUILD_DIRTY");
    emit_git_rerun_paths(&manifest_dir);
    for path in build_support::tracked_file_rerun_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // Tauri requires externalBin + resource paths to exist at build time.
    // Packaging/stage-bundle-inputs.sh overwrites these with real payloads.
    // Unit tests / plain cargo check get inert placeholders only.
    ensure_bundle_input_placeholders(&manifest_dir);
    tauri_build::build()
}

/// Create gitignored placeholder bundle inputs when packaging has not staged them yet.
fn ensure_bundle_input_placeholders(manifest_dir: &Path) {
    let triple = std::env::var("TAURI_ENV_TARGET_TRIPLE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TARGET").ok())
        .unwrap_or_else(|| {
            // Host triple fallback for local macOS Apple silicon / Intel.
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                "aarch64-apple-darwin".to_string()
            } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
                "x86_64-apple-darwin".to_string()
            } else {
                "aarch64-apple-darwin".to_string()
            }
        });

    let bin_dir = manifest_dir.join("binaries");
    let bin_path = bin_dir.join(format!("council-{triple}"));
    if !bin_path.is_file() {
        let _ = std::fs::create_dir_all(&bin_dir);
        // Inert placeholder — real packaging replaces this with target/release/council.
        let _ = std::fs::write(&bin_path, b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755));
        }
    }
    println!("cargo:rerun-if-changed={}", bin_path.display());

    let cabinets = manifest_dir
        .join("resources")
        .join("council-base")
        .join("cabinets");
    if !cabinets.is_dir() {
        let _ = std::fs::create_dir_all(&cabinets);
        let marker = cabinets.join(".placeholder");
        let _ = std::fs::write(marker, b"staged-by-build-rs-placeholder\n");
    }
    println!("cargo:rerun-if-changed={}", cabinets.display());
}

fn read_source_sha_file() -> Option<String> {
    // packaging root: .../irin-dmg-.../src/council-rs/warroom-tauri/src-tauri
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest.join("../../../../SOURCE_SHA.txt"),
        manifest.join("../../../../../SOURCE_SHA.txt"),
    ];
    for path in candidates {
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
