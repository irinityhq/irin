//! Cross-repo golden vector (the invariant, Action 3).
//!
//! Runs the EMITTER fence (`validate_directive_proposal_v1`) over the SHARED
//! corpus owned by the `sovereign-protocol` crate. The same corpus is run by
//! the gateway RECEIVER fence (`validate_proposal_v1_shape`). Any drift of this
//! repo's `ALLOWED_ACTIONS` / `ALLOWED_TOP_KEYS` / `ALLOWED_SCOPE_KEYS` flips a
//! case here; any drift of the receiver's flips it there — the spec'd mirror is
//! self-enforcing across both builds.
//!
//! The emitter fence takes a ```json-fenced synthesis string (it has no tenant
//! parameter — tenant-match is a gateway-receiver-only check), so the bare
//! proposal object from each case is wrapped in a single `json` fence before
//! the call.

use council_rs::engine::directive_fence::validate_directive_proposal_v1;
use sovereign_protocol::fence_vectors::{FenceExpect, directive_fence_golden_cases};

/// Wrap a bare proposal object in the single ```json fence the emitter expects.
fn fence(proposal: &serde_json::Value) -> String {
    format!(
        "```json\n{}\n```",
        serde_json::to_string(proposal).expect("proposal serializes")
    )
}

#[test]
fn cross_repo_golden_vector_matches_emitter_fence() {
    for case in directive_fence_golden_cases() {
        let synthesis = fence(&case.proposal);
        let result = validate_directive_proposal_v1(&synthesis);
        match case.expect {
            FenceExpect::Accept => assert!(
                result.is_ok(),
                "golden case '{}' ({}) expected ACCEPT, got {:?}",
                case.name,
                case.fault,
                result
            ),
            FenceExpect::Reject => {
                let err = result.expect_err(&format!(
                    "golden case '{}' ({}) expected REJECT, got Ok",
                    case.name, case.fault
                ));
                if let Some(sub) = &case.reason_substring {
                    assert!(
                        err.contains(sub.as_str()),
                        "golden case '{}' reject reason {:?} must contain {:?}",
                        case.name,
                        err,
                        sub
                    );
                }
            }
        }
    }
}
