use super::*;
use crate::gateway_pack::keys::write_atomic_0600;
use crate::gateway_pack::manifest::{ImageManifest, PackImages, WatchInvariants};
use crate::gateway_pack::paths::{
    bundled_pack_root, gateway_data_dir, installed_marker_path, is_pack_installed,
};
use crate::private_config::test_env_lock;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[test]
fn bundled_alone_is_not_installed() {
    let _g = test_env_lock();
    let prev = std::env::var("HOME").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-notinst-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);
    assert!(!is_pack_installed());
    assert!(installed_pack_root().is_none());
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn install_swap_replaces_stale_and_writes_marker() {
    let _g = test_env_lock();
    let prev = std::env::var("HOME").ok();
    let prev_pack = std::env::var("IRIN_GATEWAY_PACK_ROOT").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-swap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);

    let bundle = tmp.join("bundle");
    fs::create_dir_all(bundle.join("conf")).unwrap();
    fs::create_dir_all(bundle.join("lua")).unwrap();
    fs::write(
        bundle.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n",
    )
    .unwrap();
    fs::write(bundle.join("nginx.conf"), b"# nginx\n").unwrap();
    let hex = "a".repeat(64);
    let manifest = format!(
        r#"{{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "0.1.0-a",
  "source_sha": "abc",
  "source_dirty": false,
  "images": {{
"gateway": "irin-desktop/gateway@sha256:{hex}",
"sidecar": "irin-desktop/sidecar@sha256:{hex}"
  }},
  "watch_invariants": {{
"WATCH_PRODUCER_ENABLED": false,
"WATCH_DISPATCHER_ENABLED": false
  }}
}}"#
    );
    fs::write(bundle.join("image-manifest.json"), manifest.as_bytes()).unwrap();
    std::env::set_var("IRIN_GATEWAY_PACK_ROOT", &bundle);

    let dest = install_pack_files().unwrap();
    assert!(dest.join("docker-compose.yml").is_file());
    assert!(is_pack_installed());
    // Stale file then update with new manifest version.
    fs::write(dest.join("stale.txt"), b"old").unwrap();
    let manifest_b = manifest.replace("0.1.0-a", "0.1.0-b");
    fs::write(bundle.join("image-manifest.json"), manifest_b.as_bytes()).unwrap();
    let dest2 = install_pack_files().unwrap();
    assert!(
        !dest2.join("stale.txt").exists(),
        "stale file survived swap"
    );
    let marker: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(installed_marker_path()).unwrap()).unwrap();
    assert_eq!(marker["pack_version"], "0.1.0-b");

    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match prev_pack {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_ROOT", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_ROOT"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn pack_root_override_honored_in_debug_builds() {
    let _g = test_env_lock();
    let prev_pack = std::env::var("IRIN_GATEWAY_PACK_ROOT").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-override-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    fs::write(
        tmp.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n",
    )
    .unwrap();
    std::env::set_var("IRIN_GATEWAY_PACK_ROOT", &tmp);

    // cargo test builds with debug_assertions: the escape hatch applies.
    // Packaged release builds skip this branch entirely (cfg-gated), so
    // the env var can never redirect a production install.
    assert_eq!(bundled_pack_root().as_deref(), Some(tmp.as_path()));

    match prev_pack {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_ROOT", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_ROOT"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn pack_asset_integrity_restages_tampered_tree() {
    let _g = test_env_lock();
    let prev_root = std::env::var("IRIN_GATEWAY_PACK_ROOT").ok();
    let prev_support = std::env::var(crate::private_config::APP_SUPPORT_ROOT_ENV).ok();
    let uniq = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let bundle = std::env::temp_dir().join(format!("gw-pack-bundle-{uniq}"));
    let support = std::env::temp_dir().join(format!("gw-pack-support-{uniq}"));
    let _ = fs::remove_dir_all(&bundle);
    let _ = fs::remove_dir_all(&support);

    // Minimal complete bundle fixture.
    fs::create_dir_all(bundle.join("conf")).unwrap();
    fs::create_dir_all(bundle.join("lua")).unwrap();
    fs::write(
        bundle.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n",
    )
    .unwrap();
    fs::write(bundle.join("nginx.conf"), b"events {}\n").unwrap();
    fs::write(bundle.join("conf").join("gateway.conf"), b"server {}\n").unwrap();
    fs::write(bundle.join("lua").join("auth.lua"), b"-- auth\n").unwrap();
    let manifest = crate::gateway_pack::manifest::ImageManifest {
        schema_version: 1,
        mode: "local-dev".into(),
        pack_version: "0.1.0-test".into(),
        images: crate::gateway_pack::manifest::PackImages {
            gateway: format!("irin-desktop/gateway@sha256:{}", "c".repeat(64)),
            sidecar: format!("irin-desktop/sidecar@sha256:{}", "c".repeat(64)),
        },
        third_party_pins: Default::default(),
        watch_invariants: crate::gateway_pack::manifest::WatchInvariants {
            watch_producer_enabled: false,
            watch_dispatcher_enabled: false,
        },
        image_ids: Default::default(),
        local_tags: Default::default(),
        notes: None,
        source_sha: None,
        source_dirty: None,
    };
    fs::write(
        bundle.join("image-manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    std::env::set_var("IRIN_GATEWAY_PACK_ROOT", &bundle);
    std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, &support);

    let pack_root = install_pack_files().expect("install fixture pack");
    // Baseline verifies clean.
    verify_pack_asset_integrity(&pack_root).expect("fresh install verifies");

    // Tamper with the installed (user-writable) tree.
    fs::write(
        pack_root.join("nginx.conf"),
        b"events { worker_rlimit_nofile 1; }\n",
    )
    .unwrap();
    assert!(verify_pack_asset_integrity(&pack_root).is_ok());
    assert_eq!(
        fs::read(pack_root.join("nginx.conf")).unwrap(),
        b"events {}\n".to_vec(),
        "tampered file must be re-staged from the bundle"
    );

    match prev_root {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_ROOT", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_ROOT"),
    }
    match prev_support {
        Some(v) => std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, v),
        None => std::env::remove_var(crate::private_config::APP_SUPPORT_ROOT_ENV),
    }
    let _ = fs::remove_dir_all(&bundle);
    let _ = fs::remove_dir_all(&support);
}

/// App upgrade with an unchanged pack version: the installed tree still matches
/// its install marker, but no longer matches the new bundle. The integrity pass
/// must treat that drift like tamper and re-stage from the bundle.
#[test]
fn pack_asset_integrity_restages_bundle_drift() {
    let _g = test_env_lock();
    let prev_root = std::env::var("IRIN_GATEWAY_PACK_ROOT").ok();
    let prev_support = std::env::var(crate::private_config::APP_SUPPORT_ROOT_ENV).ok();
    let uniq = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let bundle = std::env::temp_dir().join(format!("gw-pack-bundle-drift-{uniq}"));
    let support = std::env::temp_dir().join(format!("gw-pack-support-drift-{uniq}"));
    let _ = fs::remove_dir_all(&bundle);
    let _ = fs::remove_dir_all(&support);

    fs::create_dir_all(bundle.join("conf")).unwrap();
    fs::create_dir_all(bundle.join("lua")).unwrap();
    fs::write(
        bundle.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n",
    )
    .unwrap();
    fs::write(bundle.join("nginx.conf"), b"events {}\n").unwrap();
    fs::write(bundle.join("conf").join("gateway.conf"), b"server {}\n").unwrap();
    fs::write(bundle.join("lua").join("auth.lua"), b"-- auth\n").unwrap();
    let manifest = crate::gateway_pack::manifest::ImageManifest {
        schema_version: 1,
        mode: "local-dev".into(),
        pack_version: "0.1.0-test".into(),
        images: crate::gateway_pack::manifest::PackImages {
            gateway: format!("irin-desktop/gateway@sha256:{}", "d".repeat(64)),
            sidecar: format!("irin-desktop/sidecar@sha256:{}", "d".repeat(64)),
        },
        third_party_pins: Default::default(),
        watch_invariants: crate::gateway_pack::manifest::WatchInvariants {
            watch_producer_enabled: false,
            watch_dispatcher_enabled: false,
        },
        image_ids: Default::default(),
        local_tags: Default::default(),
        notes: None,
        source_sha: None,
        source_dirty: None,
    };
    fs::write(
        bundle.join("image-manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    std::env::set_var("IRIN_GATEWAY_PACK_ROOT", &bundle);
    std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, &support);

    let pack_root = install_pack_files().expect("install fixture pack");
    verify_pack_asset_integrity(&pack_root).expect("fresh install verifies");

    // Simulate an app upgrade: the bundle's compose changes (same pack
    // version), the installed tree is untouched and still matches its marker.
    fs::write(
        bundle.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n# upgraded\n",
    )
    .unwrap();
    assert!(verify_pack_asset_integrity(&pack_root).is_ok());
    assert_eq!(
        fs::read(pack_root.join("docker-compose.yml")).unwrap(),
        b"name: irin-desktop-gateway\n# upgraded\n".to_vec(),
        "stale installed compose must be re-staged from the upgraded bundle"
    );

    match prev_root {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_ROOT", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_ROOT"),
    }
    match prev_support {
        Some(v) => std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, v),
        None => std::env::remove_var(crate::private_config::APP_SUPPORT_ROOT_ENV),
    }
    let _ = fs::remove_dir_all(&bundle);
    let _ = fs::remove_dir_all(&support);
}
