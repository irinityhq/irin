use super::*;
use crate::docker_cli::ComposeEnv;
use crate::gateway_pack::keys::{serialize_public_env, validate_env_value};
use crate::gateway_pack::manifest::{ImageRef, ManifestMode, ValidatedManifest};
use crate::gateway_pack::paths::{
    arm_keys_path, ensure_watch_dirs, gateway_data_dir, public_env_path, sentinels_dir,
    watch_inbox_dir, watch_profile_path, ARM_KEYS_CONTAINER_PATH, WATCH_PROFILE_CONTAINER_PATH,
};
use crate::keychain::{MemorySecretStore, SecretStore, ARM_PRINCIPAL_NAME};
use crate::private_config::test_env_lock;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[test]
fn secret_env_never_in_public_file_shape() {
    let body = serialize_public_env(&[
        ("IRIN_GATEWAY_IMAGE".into(), "x".into()),
        ("BOOTSTRAP_TOKEN".into(), "".into()),
    ])
    .unwrap();
    assert!(!body.contains("AUTH_PEPPER="));
    assert!(!body.contains("WATCH_ADMIN_TOKEN"));
    assert!(!body.contains("COUNCIL_GATEWAY_TOKEN"));
    // Empty bootstrap is ok in public file (blanked).
    assert!(body.contains("BOOTSTRAP_TOKEN="));
}

#[test]
fn teardown_compose_env_never_loads_keychain_or_provider_secrets() {
    let _g = test_env_lock();
    let prev_skip = std::env::var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").ok();
    let prev_support = std::env::var(crate::private_config::APP_SUPPORT_ROOT_ENV).ok();
    std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", "1");
    std::env::set_var("XAI_API_KEY", "should-not-appear-in-teardown");

    let uniq = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let support = std::env::temp_dir().join(format!("gw-teardown-support-{uniq}"));
    let pack = support.join("gateway").join("pack");
    let _ = fs::remove_dir_all(&support);
    fs::create_dir_all(&pack).unwrap();
    std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, &support);

    fs::write(
        pack.join("docker-compose.yml"),
        b"name: irin-desktop-gateway\n",
    )
    .unwrap();
    let manifest = crate::gateway_pack::manifest::ImageManifest {
        schema_version: 1,
        mode: "local-dev".into(),
        pack_version: "0.1.0-teardown".into(),
        images: crate::gateway_pack::manifest::PackImages {
            gateway: format!("irin-desktop/gateway@sha256:{}", "a".repeat(64)),
            sidecar: format!("irin-desktop/sidecar@sha256:{}", "a".repeat(64)),
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
        pack.join("image-manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let store = MemorySecretStore::default();
    let pepper = "ab".repeat(32);
    crate::keychain::store_auth_pepper(&store, &pepper).unwrap();
    let key = format!("gw_{}", "f".repeat(32));
    crate::keychain::store_gw_api_key(&store, &key).unwrap();

    let env = teardown_compose_env(&pack, None);
    assert_eq!(
        env.get("AUTH_PEPPER").map(String::as_str),
        Some(""),
        "teardown must not load Keychain AUTH_PEPPER"
    );
    assert_eq!(env.get("BOOTSTRAP_TOKEN").map(String::as_str), Some(""));
    assert_eq!(env.get("XAI_API_KEY").map(String::as_str), Some(""));
    assert_eq!(env.get("CLAUDE_PROXY_TOKEN").map(String::as_str), Some(""));
    assert_eq!(env.get("CODEX_PROXY_TOKEN").map(String::as_str), Some(""));
    assert_eq!(env.get("CLAUDE_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(env.get("CODEX_PROXY_URL").map(String::as_str), Some(""));
    for secret in [
        pepper.as_str(),
        key.as_str(),
        "should-not-appear-in-teardown",
    ] {
        for v in env.values() {
            assert!(
                !v.contains(secret),
                "teardown env leaked secret material: {v}"
            );
        }
    }
    assert!(
        env.get("IRIN_GATEWAY_IMAGE")
            .map(|s| s.contains("sha256:"))
            .unwrap_or(false),
        "pins should still load from installed manifest: {env:?}"
    );

    match prev_skip {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV"),
    }
    match prev_support {
        Some(v) => std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, v),
        None => std::env::remove_var(crate::private_config::APP_SUPPORT_ROOT_ENV),
    }
    std::env::remove_var("XAI_API_KEY");
    let _ = fs::remove_dir_all(&support);
}

fn test_image_ref(name: &str, hex: &str) -> ImageRef {
    ImageRef::parse(&format!("{name}@sha256:{}", hex.repeat(64))).unwrap()
}

#[test]
fn pin_env_forces_manifest_and_app_paths_over_ambient_decoys() {
    let _g = test_env_lock();
    let decoys = [
        ("IRIN_GATEWAY_IMAGE", "evil.example/swapped-gateway:latest"),
        ("IRIN_SIDECAR_IMAGE", "evil.example/swapped-sidecar:latest"),
        ("IRIN_DESKTOP_PACK_ROOT", "/tmp/evil-pack"),
        ("IRIN_DESKTOP_LEDGER_KEY", "/tmp/evil-ledger"),
        ("GATEWAY_AUTH_FAIL_CLOSED", "false"),
        ("GW_ENABLE_STREAMING", "1"),
        ("GW_ENABLE_BATCH", "1"),
        ("GATEWAY_BASE_URL", "http://evil.example:9999"),
    ];
    for (k, v) in &decoys {
        std::env::set_var(k, v);
    }

    let gateway = test_image_ref("ghcr.io/irin/gateway", "a");
    let sidecar = test_image_ref("ghcr.io/irin/sidecar", "b");
    let pins = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &gateway,
        &sidecar,
        Some("k_abcdef12"),
    )
    .unwrap();

    // Pins come from the validated manifest and app-owned paths, never
    // from the ambient parent environment.
    assert_eq!(
        pins.get("IRIN_GATEWAY_IMAGE").map(String::as_str),
        Some(gateway.as_str())
    );
    assert_eq!(
        pins.get("IRIN_SIDECAR_IMAGE").map(String::as_str),
        Some(sidecar.as_str())
    );
    assert_eq!(
        pins.get("IRIN_DESKTOP_PACK_ROOT").map(String::as_str),
        Some("/app/pack")
    );
    assert_eq!(
        pins.get("IRIN_DESKTOP_LEDGER_KEY").map(String::as_str),
        Some("/app/ledger")
    );
    assert_eq!(
        pins.get("GATEWAY_AUTH_FAIL_CLOSED").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        pins.get("GW_ENABLE_STREAMING").map(String::as_str),
        Some("0")
    );
    assert_eq!(pins.get("GW_ENABLE_BATCH").map(String::as_str), Some("0"));
    assert_eq!(
        pins.get("GATEWAY_BASE_URL").map(String::as_str),
        Some("http://gateway:8080")
    );
    assert_eq!(
        pins.get("WATCH_PRODUCER_ENABLED").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        pins.get("WATCH_DISPATCHER_ENABLED").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        pins.get("COUNCIL_GATEWAY_KEY_ID").map(String::as_str),
        Some("k_abcdef12")
    );
    // The pin set never carries disarmed-surface or secret material.
    assert!(!pins.contains_key("WATCH_ADMIN_TOKEN"));
    assert!(!pins.contains_key("COUNCIL_GATEWAY_TOKEN"));
    assert!(!pins.contains_key("AUTH_PEPPER"));

    for (k, _) in &decoys {
        std::env::remove_var(k);
    }
}

/// Every pin the pack env-builder emits must pass the compose spawn-env
/// allowlist, or enable/resume fails at validate_compose_invocation with
/// "env key not allow-listed" before docker is even invoked.
#[test]
fn every_pack_pin_key_is_compose_allow_listed() {
    let _g = test_env_lock();
    let gateway = test_image_ref("ghcr.io/irin/gateway", "a");
    let sidecar = test_image_ref("ghcr.io/irin/sidecar", "b");
    let pins = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &gateway,
        &sidecar,
        Some("k_abcdef12"),
    )
    .unwrap();
    for key in pins.keys() {
        assert!(
            crate::docker_cli::COMPOSE_ENV_KEY_ALLOWLIST.contains(&key.as_str()),
            "pack pin {key} would be rejected at compose spawn — add it to COMPOSE_ENV_KEY_ALLOWLIST"
        );
    }
}

#[test]
fn full_compose_env_secrets_win_over_pin_defaults() {
    let _g = test_env_lock();
    let prev_skip = std::env::var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").ok();
    std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", "1");
    let store = MemorySecretStore::default();
    let validated = ValidatedManifest {
        mode: ManifestMode::LocalDev,
        pack_version: "0.1.0-test".into(),
        gateway: test_image_ref("irin-desktop/gateway", "c"),
        sidecar: test_image_ref("irin-desktop/sidecar", "d"),
        third_party: vec![],
        source_sha: None,
        source_dirty: None,
        local_image_ids: Default::default(),
    };
    let env = build_full_compose_env(
        &store,
        Some("bootstrap-hex"),
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &validated,
        None,
        None,
    )
    .unwrap();
    // Secret env overrides the blank pin default for bootstrap.
    assert_eq!(
        env.get("BOOTSTRAP_TOKEN").map(String::as_str),
        Some("bootstrap-hex")
    );
    assert!(env
        .get("AUTH_PEPPER")
        .map(|p| !p.is_empty())
        .unwrap_or(false));
    // Pins are still present in the merged spawn env.
    assert_eq!(
        env.get("IRIN_GATEWAY_IMAGE").map(String::as_str),
        Some(validated.gateway.as_str())
    );
    assert_eq!(
        env.get("IRIN_DESKTOP_PACK_ROOT").map(String::as_str),
        Some("/app/pack")
    );
    // Proxy slots always forced (empty when adapters unready).
    assert!(env.contains_key("CLAUDE_PROXY_URL"));
    assert!(env.contains_key("CODEX_PROXY_URL"));
    assert!(env.contains_key("CLAUDE_PROXY_TOKEN"));
    assert!(env.contains_key("CODEX_PROXY_TOKEN"));
    match prev_skip {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV"),
    }
}

#[test]
fn proxy_tokens_never_reach_public_env_file() {
    let _g = test_env_lock();
    let prev_support = std::env::var(crate::private_config::APP_SUPPORT_ROOT_ENV).ok();
    let uniq = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let support = std::env::temp_dir().join(format!("gw-proxy-public-{uniq}"));
    let _ = fs::remove_dir_all(&support);
    fs::create_dir_all(&support).unwrap();
    std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, &support);

    let store = MemorySecretStore::default();
    let (claude_tok, codex_tok) = super::super::cli_adapters::ensure_proxy_tokens(&store).unwrap();
    let path = write_public_compose_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "a"),
        &test_image_ref("ghcr.io/irin/sidecar", "b"),
        None,
    )
    .unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(
        !body.contains(&claude_tok) && !body.contains(&codex_tok),
        "public env must never contain proxy tokens"
    );
    assert!(!body.contains("CLAUDE_PROXY_TOKEN="));
    assert!(!body.contains("CODEX_PROXY_TOKEN="));
    assert!(!body.contains("CLAUDE_PROXY_URL="));
    assert!(!body.contains("CODEX_PROXY_URL="));

    match prev_support {
        Some(v) => std::env::set_var(crate::private_config::APP_SUPPORT_ROOT_ENV, v),
        None => std::env::remove_var(crate::private_config::APP_SUPPORT_ROOT_ENV),
    }
    let _ = fs::remove_dir_all(&support);
}

/// Extract `${VAR…}` names from compose YAML (handles `:-`, `:?`, `}` end).
fn compose_interpolated_vars(raw: &str) -> std::collections::BTreeSet<String> {
    let bytes = raw.as_bytes();
    let mut out = std::collections::BTreeSet::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start {
                out.insert(raw[start..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Touch ID bridge: the arm-principal registry exists ONLY when the
/// Keychain holds a token, and it never reaches the public env file.
#[test]
fn arm_principals_come_from_the_keychain_and_never_the_env_file() {
    let _g = test_env_lock();
    let prev_skip = std::env::var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").ok();
    std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", "1");
    let store = MemorySecretStore::default();

    // No token stored: the registry string is empty, so the sidecar boots
    // with zero principals and every arm route 401s.
    let env = build_compose_secret_env(&store, None, None).unwrap();
    assert_eq!(env.get("GW_ARM_PRINCIPALS").map(String::as_str), Some(""));

    // With a token: `<name>:<token>`, and the name is the audit label.
    let token = format!("tok_{:032x}", std::process::id());
    crate::keychain::store_arm_principal_token(&store, &token).unwrap();
    let env = build_compose_secret_env(&store, None, None).unwrap();
    let registry = env.get("GW_ARM_PRINCIPALS").cloned().unwrap_or_default();
    assert_eq!(registry, format!("irin-desktop:{token}"));
    // No separator or injection byte can appear inside the value.
    assert_eq!(registry.matches(':').count(), 1);
    assert!(!registry.contains(',') && !registry.contains('\n'));

    // The PUBLIC env file carries the non-secret path pins and nothing else.
    let pins = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "e"),
        &test_image_ref("ghcr.io/irin/sidecar", "f"),
        None,
    )
    .unwrap();
    assert!(
        !pins.contains_key("GW_ARM_PRINCIPALS"),
        "the arm-principal registry is never a public pin"
    );
    assert_eq!(
        pins.get("GW_ARM_ATTEST_KEYS_PATH").map(String::as_str),
        Some(ARM_KEYS_CONTAINER_PATH)
    );
    assert_eq!(
        pins.get("IRIN_DESKTOP_ARM_KEYS").map(String::as_str),
        Some(arm_keys_path().display().to_string().as_str())
    );

    let body = serialize_public_env(
        &pins
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!body.contains("GW_ARM_PRINCIPALS"));
    assert!(!body.contains("tok_"));

    match prev_skip {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV"),
    }
}

/// The watch-admin read token is minted once into the Keychain and reused on
/// later Enable runs; it never becomes a public pin.
#[test]
fn watch_admin_token_minted_once_and_never_a_public_pin() {
    let _g = test_env_lock();
    let prev_skip = std::env::var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").ok();
    std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", "1");
    let store = MemorySecretStore::default();

    let first = build_compose_secret_env(&store, None, None).unwrap();
    let minted = first.get("WATCH_ADMIN_TOKEN").cloned().unwrap_or_default();
    assert!(crate::keychain::is_valid_watch_admin_token(&minted));

    // A later Enable run reuses the Keychain item instead of rotating it.
    let second = build_compose_secret_env(&store, None, None).unwrap();
    assert_eq!(
        second.get("WATCH_ADMIN_TOKEN").map(String::as_str),
        Some(minted.as_str())
    );

    let pins = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "e"),
        &test_image_ref("ghcr.io/irin/sidecar", "f"),
        None,
    )
    .unwrap();
    assert!(
        !pins.contains_key("WATCH_ADMIN_TOKEN"),
        "the watch-admin read token is never a public pin"
    );

    match prev_skip {
        Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV", v),
        None => std::env::remove_var("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV"),
    }
}

/// Teardown never carries the arm-principal registry, and still pins the
/// paths Compose must interpolate for `down`.
#[test]
fn teardown_env_drops_arm_principals_but_keeps_path_pins() {
    let env = teardown_compose_env(Path::new("/app/pack"), None);
    assert_eq!(env.get("GW_ARM_PRINCIPALS").map(String::as_str), Some(""));
    assert_eq!(
        env.get("GW_ARM_ATTEST_KEYS_PATH").map(String::as_str),
        Some(ARM_KEYS_CONTAINER_PATH)
    );
    assert!(env.contains_key("IRIN_DESKTOP_ARM_KEYS"));
    assert!(env.contains_key("IRIN_DESKTOP_SENTINELS_DIR"));
    assert!(env.contains_key("IRIN_DESKTOP_WATCH_INBOX_DIR"));
    assert!(env.contains_key("IRIN_WATCH_PROFILE_PATH"));
}

/// Watch profile/inbox pins always present; IRIN_WATCH_PROFILE_PATH is empty
/// when no profile file is installed and the container path when it is.
#[test]
fn watch_profile_pins_empty_without_file_and_set_when_present() {
    let _g = test_env_lock();
    ensure_watch_dirs().unwrap();
    let profile = watch_profile_path();
    let _ = fs::remove_file(&profile);

    let pins_off = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "e"),
        &test_image_ref("ghcr.io/irin/sidecar", "f"),
        None,
    )
    .unwrap();
    assert_eq!(
        pins_off
            .get("IRIN_DESKTOP_SENTINELS_DIR")
            .map(String::as_str),
        Some(sentinels_dir().display().to_string().as_str())
    );
    assert_eq!(
        pins_off
            .get("IRIN_DESKTOP_WATCH_INBOX_DIR")
            .map(String::as_str),
        Some(watch_inbox_dir().display().to_string().as_str())
    );
    assert_eq!(
        pins_off.get("IRIN_WATCH_PROFILE_PATH").map(String::as_str),
        Some("")
    );

    fs::write(&profile, "placeholder\n").unwrap();
    let pins_on = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "e"),
        &test_image_ref("ghcr.io/irin/sidecar", "f"),
        None,
    )
    .unwrap();
    assert_eq!(
        pins_on.get("IRIN_WATCH_PROFILE_PATH").map(String::as_str),
        Some(WATCH_PROFILE_CONTAINER_PATH)
    );
    let _ = fs::remove_file(&profile);
}

#[test]
fn ensure_watch_dirs_creates_sentinels_and_inbox() {
    let _g = test_env_lock();
    // Remove dirs if present so we prove create path.
    let s = sentinels_dir();
    let i = watch_inbox_dir();
    let _ = fs::remove_dir_all(&s);
    let _ = fs::remove_dir_all(&i);
    ensure_watch_dirs().unwrap();
    assert!(s.is_dir());
    assert!(i.is_dir());
}

#[test]
fn every_compose_interpolated_var_is_pinned_scrubbed_or_disarmed() {
    // Every variable the pack compose file interpolates must be forced by
    // a spawn-env layer — pins (validated manifest/app paths), the secret
    // env (Keychain/login), or the docker_cli disarm force — because
    // Compose ranks process env above --env-file and ambient parent
    // values would otherwise win.
    let compose = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packaging/gateway-pack/docker-compose.yml");
    if !compose.is_file() {
        eprintln!(
            "skipping compose interpolation audit: {} not present",
            compose.display()
        );
        return;
    }
    let raw = fs::read_to_string(&compose).unwrap();
    let pins = build_pack_pin_env(
        Path::new("/app/pack"),
        Path::new("/app/ledger"),
        &test_image_ref("ghcr.io/irin/gateway", "e"),
        &test_image_ref("ghcr.io/irin/sidecar", "f"),
        Some("k_abcdef12"),
    )
    .unwrap();
    // Keys forced by the secret env or the docker_cli scrub/disarm layers.
    let secret_or_disarm: std::collections::BTreeSet<&str> = [
        "XAI_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "NVIDIA_API_KEY",
        "AUTH_PEPPER",
        "BOOTSTRAP_TOKEN",
        // Watch/Outbox admin read token: Keychain-held, supplied by the secret
        // env layer and scrubbed from the ambient environment before it.
        "WATCH_ADMIN_TOKEN",
        "COUNCIL_GATEWAY_TOKEN",
        "WATCH_PRODUCER_ENABLED",
        "WATCH_DISPATCHER_ENABLED",
        // Touch ID bridge: Keychain-held, supplied by the secret env layer
        // and scrubbed from the ambient environment before it.
        "GW_ARM_PRINCIPALS",
        // Host CLI adapters: secret env layer (empty when unready).
        "CLAUDE_PROXY_URL",
        "CODEX_PROXY_URL",
        "CLAUDE_PROXY_TOKEN",
        "CODEX_PROXY_TOKEN",
    ]
    .into_iter()
    .collect();
    let interpolated = compose_interpolated_vars(&raw);
    assert!(
        interpolated.contains("IRIN_GATEWAY_IMAGE"),
        "audit sanity: compose must interpolate the image pins"
    );
    for var in interpolated {
        assert!(
            pins.contains_key(&var) || secret_or_disarm.contains(var.as_str()),
            "compose interpolates ${var} but no spawn-env layer forces it"
        );
    }
}
