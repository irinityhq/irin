#![feature(rustc_private)]
#![warn(unused_extern_crates)]

dylint_linting::dylint_library!();

extern crate rustc_ast;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

mod no_debug_on_signing_key_types;
mod prefer_subtle_ct_eq;

#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    no_debug_on_signing_key_types::register(lint_store);
    prefer_subtle_ct_eq::register(lint_store);
}

/// Name looks like signing / key-material types (case-insensitive substrings).
pub(crate) fn is_key_material_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("signingkey")
        || lower.contains("keymaterial")
        || lower.contains("secretkey")
        || lower.contains("privatekey")
        || lower.contains("ledgerkey")
}

/// Function name is in a crypto/auth comparison surface.
pub(crate) fn is_sensitive_cmp_fn(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("verify")
        || lower.contains("compare")
        || lower.contains("auth")
        || lower.contains("arm")
        || lower.contains("token")
        || lower.contains("mac")
        || lower.contains("sig")
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn key_material_names() {
        assert!(is_key_material_name("SigningKey"));
        assert!(is_key_material_name("LedgerKey"));
        assert!(is_key_material_name("MySecretKeyBytes"));
        assert!(is_key_material_name("privatekey"));
        // Underscore form does not collapse; prefer CamelCase type names.
        assert!(!is_key_material_name("ledger_key"));
        assert!(!is_key_material_name("PublicKey"));
        assert!(!is_key_material_name("Config"));
    }

    #[test]
    fn sensitive_cmp_fns() {
        assert!(is_sensitive_cmp_fn("verify_mac"));
        assert!(is_sensitive_cmp_fn("compare_tokens"));
        assert!(is_sensitive_cmp_fn("authenticate"));
        assert!(is_sensitive_cmp_fn("arm_watch"));
        assert!(!is_sensitive_cmp_fn("format_display"));
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
