// Shared type definitions for the cross-component directive-fence golden corpus.
//
// This file is the SINGLE definition of `FenceExpect` + `DirectiveFenceCase`,
// `include!`d from two places so they can never drift:
//   - `src/fence_vectors.rs` (the library module — re-exports these as
//     `sovereign_protocol::fence_vectors::{FenceExpect, DirectiveFenceCase}`)
//   - `build.rs` (the compile-time corpus guard, which deserializes the corpus
//     through THIS struct so the build enforces exactly the loader's schema —
//     `#[serde(deny_unknown_fields)]`, required fields, the `expect` enum)
//
// It therefore uses fully-qualified `serde::`/`serde_json::` paths and declares
// no `use` items, so it composes cleanly into either includer regardless of that
// file's imports. Keep it dependency-free beyond serde/serde_json.

/// The polarity a fence MUST produce for a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FenceExpect {
    /// The fence accepts the proposal (`Ok(())`).
    Accept,
    /// The fence rejects the proposal (`Err(..)`).
    Reject,
}

/// One cross-component golden case. See `fence_vectors.rs` module docs for the contract.
///
/// `deny_unknown_fields`: a misspelled or stale authoring key in the shared
/// corpus is a hard parse error, not a silently-dropped field — so a consumer
/// can never run against a case whose intent was quietly lost.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveFenceCase {
    /// Stable, unique case id (used in assertion messages).
    pub name: String,
    /// Human note: the single fault (reject) or why the case is valid (accept).
    pub fault: String,
    /// Tenant the RECEIVER fence must be given as `expected_tenant`. Every Act
    /// case sets `scope.tenant` to this so accept-cases accept on both fences.
    pub tenant: String,
    /// Expected polarity for BOTH fences.
    pub expect: FenceExpect,
    /// Verified common substring of BOTH fences' reject messages; `None` for
    /// accept cases. When present, each adapter also asserts the error contains
    /// this substring (pins the reject CATEGORY, not just the polarity).
    #[serde(default)]
    pub reason_substring: Option<String>,
    /// The bare proposal object — NOT fence-wrapped. The gateway adapter feeds
    /// it as a `serde_json::Value`; the council-rs adapter wraps it in a single
    /// fenced `json` block before calling its string-input fence.
    pub proposal: serde_json::Value,
}
