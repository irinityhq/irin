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
    println!("cargo:rerun-if-env-changed=IRIN_COUNCIL_PORT");
    println!("cargo:rerun-if-env-changed=TAURI_CONFIG");
    let council_port = std::env::var("IRIN_COUNCIL_PORT")
        .ok()
        .filter(|raw| !raw.trim().is_empty())
        .unwrap_or_else(|| "8765".to_string());
    let parsed_port = council_port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .unwrap_or_else(|| {
            panic!("IRIN_COUNCIL_PORT must be a non-zero TCP port (got {council_port:?})")
        });
    if parsed_port != 8765 {
        let tauri_config = std::env::var("TAURI_CONFIG").unwrap_or_default();
        let required_origins = [
            format!("http://127.0.0.1:{parsed_port}"),
            format!("ws://127.0.0.1:{parsed_port}"),
        ];
        assert!(
            required_origins
                .iter()
                .all(|origin| tauri_config.contains(origin)),
            "a non-default IRIN_COUNCIL_PORT requires TAURI_CONFIG with exact \
             HTTP and WebSocket CSP origins for that port"
        );
    }
    println!("cargo:rustc-env=IRIN_TAURI_COUNCIL_PORT={parsed_port}");
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

    // Gateway pack resources (compose + conf/lua staged by scripts/stage-gateway-pack.sh).
    let gw_pack = manifest_dir.join("resources").join("gateway-pack");
    let gw_compose = gw_pack.join("docker-compose.yml");
    if !gw_compose.is_file() {
        let _ = std::fs::create_dir_all(&gw_pack);
        let _ = std::fs::write(
            &gw_compose,
            b"# placeholder - run scripts/stage-gateway-pack.sh before DMG build\nname: irin-desktop-gateway\nservices: {}\n",
        );
        let _ = std::fs::write(
            gw_pack.join("image-manifest.json"),
            br#"{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "placeholder",
  "images": {
    "gateway": "irin-desktop/gateway@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    "sidecar": "irin-desktop/sidecar@sha256:0000000000000000000000000000000000000000000000000000000000000000"
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": false,
    "WATCH_DISPATCHER_ENABLED": false
  }
}
"#,
        );
    }
    println!("cargo:rerun-if-changed={}", gw_compose.display());
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
