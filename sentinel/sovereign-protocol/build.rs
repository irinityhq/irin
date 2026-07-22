//! Build-time guard for the cross-component directive-fence golden corpus
//! (`src/vectors/directive_fence_cases.json`, the invariant).
//!
//! The corpus is the single accept/reject SSOT consumed by THREE fence tests:
//! this crate's `fence_vectors_golden`, the gateway RECEIVER fence, and the
//! council-rs EMITTER fence. A corrupt or mis-authored corpus must fail HERE,
//! once, at compile time — not surface later as a confusing test failure inside
//! a downstream consumer that merely *depends* on this crate. Because `build.rs`
//! runs on every `cargo build` of the crate (including downstream consumer
//! builds), this gate is strictly earlier and harder than the equivalent
//! `tests/fence_vectors_golden.rs` checks, which only run under `cargo test`.
//!
//! Schema: this deserializes the corpus through the REAL `DirectiveFenceCase`
//! (`include!`d from `src/fence_case.rs`, the same definition the library and
//! its loader use), so the build enforces exactly the loader's schema —
//! `#[serde(deny_unknown_fields)]`, every required field (incl. `fault`), and
//! the `expect` enum. No hand-maintained mirror, so nothing to drift; and
//! `fence_case.rs` is in the `rerun-if-changed` set, so a struct edit re-runs
//! this guard rather than reusing a stale build-script result.
//!
//! On top of the typed parse it asserts the same VALUE invariants as the golden
//! test: the non-trivial floor (>=5 cases), at least one accept and one reject,
//! polarity <-> `reason_substring`, proposal-is-object, unique names, and the
//! Act `scope.tenant == case.tenant` receiver-adapter contract.
//!
//! Rollback: delete this file, `src/fence_case.rs`'s `include!` wiring stays for
//! the library, and the `[build-dependencies]` block in `Cargo.toml`. The same
//! invariants still hold at test time via `tests/fence_vectors_golden.rs`; only
//! the build-time + downstream-build gate is lost.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

// The corpus schema — the exact struct the library + loader use. Bringing in the
// real definition (rather than a copy) is what makes the build-time schema check
// drift-proof.
include!("src/fence_case.rs");

/// Corpus path, relative to `CARGO_MANIFEST_DIR`.
const CORPUS: &str = "src/vectors/directive_fence_cases.json";
/// Shared schema path, relative to `CARGO_MANIFEST_DIR`.
const SCHEMA: &str = "src/fence_case.rs";

fn main() {
    println!("cargo:rerun-if-changed={CORPUS}");
    println!("cargo:rerun-if-changed={SCHEMA}");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let path = Path::new(&manifest_dir).join(CORPUS);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "directive-fence corpus unreadable at {}: {e}",
            path.display()
        )
    });

    if let Err(e) = validate(&raw) {
        panic!("directive-fence golden corpus invalid ({CORPUS}): {e}");
    }
}

/// Deserialize through the real `DirectiveFenceCase` (enforcing the loader's full
/// schema), then assert the golden test's VALUE invariants. Returns the first
/// violation (build-fatal) or `Ok(())`.
fn validate(raw: &str) -> Result<(), String> {
    // Typed parse = the loader's schema gate: deny_unknown_fields, every required
    // field (incl. `fault`), and a valid `expect` enum, all at build time.
    let cases: Vec<DirectiveFenceCase> = serde_json::from_str(raw)
        .map_err(|e| format!("corpus does not match DirectiveFenceCase schema: {e}"))?;

    // Non-trivial floor — mirrors corpus_parses_and_is_non_trivial. Reject any
    // truncation, not just an empty corpus.
    const MIN_CASES: usize = 5;
    if cases.len() < MIN_CASES {
        return Err(format!(
            "corpus must carry at least {MIN_CASES} cases (a truncated or vacuous \
             corpus would let consumers' mirror tests pass without real coverage), \
             got {}",
            cases.len()
        ));
    }

    let mut seen = HashSet::new();
    let mut saw_accept = false;
    let mut saw_reject = false;

    for c in &cases {
        if !seen.insert(c.name.as_str()) {
            return Err(format!("duplicate case name {:?}", c.name));
        }

        match c.expect {
            FenceExpect::Accept => {
                saw_accept = true;
                if c.reason_substring.is_some() {
                    return Err(format!(
                        "accept case {:?} must NOT carry a reason_substring",
                        c.name
                    ));
                }
            }
            FenceExpect::Reject => {
                saw_reject = true;
                if c.reason_substring.as_deref().is_none_or(|s| s.is_empty()) {
                    return Err(format!(
                        "reject case {:?} must carry a non-empty reason_substring",
                        c.name
                    ));
                }
            }
        }

        if !c.proposal.is_object() {
            return Err(format!("case {:?} proposal must be a JSON object", c.name));
        }

        // Receiver-adapter contract: an Act case only accepts when scope.tenant
        // equals the tenant the adapter passes as expected_tenant, so every Act
        // case MUST set scope.tenant to its own case tenant.
        if c.proposal.get("verdict").and_then(|v| v.as_str()) == Some("Act") {
            let scope_tenant = c
                .proposal
                .get("scope")
                .and_then(|s| s.get("tenant"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| format!("Act case {:?} must set scope.tenant", c.name))?;
            if scope_tenant != c.tenant {
                return Err(format!(
                    "Act case {:?} scope.tenant {scope_tenant:?} must equal case tenant {:?}",
                    c.name, c.tenant
                ));
            }
        }
    }

    if !saw_accept {
        return Err("corpus must contain at least one accept case".into());
    }
    if !saw_reject {
        return Err("corpus must contain at least one reject case".into());
    }

    Ok(())
}
