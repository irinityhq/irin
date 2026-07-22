//! Direct-fire mode specs — single-model strikes, no council.
//!
//! One source of truth for the five direct-fire personas, shared by:
//! - the CLI handlers (`--contrarian`, `--munger`, `--kiss-review`,
//!   `--specops`, `--premortem`) in main.rs
//! - streaming escalations (`stream::deliberate::run_escalation`)
//! - the WS `direct_fire` single-shot path (feature contract)
//!
//! Transport IDs are explicit: Grok personas use the Hermes OAuth adapter and
//! KISS uses the Claude Code subscription CLI.

use crate::types::SessionMode;

/// A direct-fire persona: system prompt + provider/model assignment.
#[derive(Debug)]
pub struct DirectFireSpec {
    /// Wire slug used on the WS start payload (`direct_fire: "munger"`).
    pub slug: &'static str,
    /// Human banner, e.g. "MUNGER MIND" — matches the CLI eprintln headers.
    pub display: &'static str,
    pub system: &'static str,
    pub provider: &'static str,
    pub model: &'static str,
}

/// Valid `direct_fire` wire values (pinned WS contract, feature contract).
pub const DIRECT_FIRE_MODES: &[&str] = &["contrarian", "munger", "kiss", "specops", "premortem"];

static SPECS: &[DirectFireSpec] = &[
    DirectFireSpec {
        slug: "contrarian",
        display: "CONTRARIAN",
        system: "You are a first-principles contrarian. Tear this idea down to \
                 physical reality and economic fundamentals. No appeals to \
                 authority, no consensus. Be brutal, be specific, cite numbers.",
        provider: "grok_hermes",
        model: "grok-4.3",
    },
    DirectFireSpec {
        slug: "munger",
        display: "MUNGER MIND",
        system: "You are the Munger Mind: Charlie Munger's cognitive patterns \
                 distilled — blunt, multidisciplinary, inversion-obsessed. Apply \
                 the latticework. Always invert first: 'How would I guarantee \
                 failure?' Use second-order thinking, incentives, lollapalooza \
                 effects, circle of competence. Name 2-3 mental models applied \
                 and one way the plan could die.",
        provider: "grok_hermes",
        model: "grok-4.3",
    },
    DirectFireSpec {
        slug: "kiss",
        display: "KISS REVIEW",
        system: "You are the most intelligent analyst available. Give a direct, \
                 comprehensive answer. No committee, no rounds — just your best \
                 single-pass analysis. Structure: 1) Assessment 2) Key Risks \
                 3) Recommendation 4) Confidence level.",
        provider: "claude_code",
        model: "claude-opus-4-8",
    },
    DirectFireSpec {
        slug: "specops",
        display: "SPECOPS",
        system: "You are a SpecOps analyst — a native multi-agent swarm. \
                 Find the signal in noisy deliberation. Be surgical. \
                 One paragraph. No preamble.",
        provider: "grok_hermes",
        model: "grok-4.3",
    },
    DirectFireSpec {
        slug: "premortem",
        display: "PREMORTEM",
        system: "It is 6 months from now. The plan described below was implemented \
                 exactly as proposed — and it failed. Not a minor setback: a \
                 genuine, consequential failure. \n\n\
                 Write the After-Action Review (AAR). Use past tense throughout. \n\n\
                 Structure: \n\
                 1. TIMELINE: What happened, week by week \n\
                 2. ROOT CAUSES: The 2-3 things that actually killed it \n\
                 3. EARLY WARNINGS: Signals that were visible at decision time but ignored \n\
                 4. WHAT WE'D DO DIFFERENTLY: Concrete changes, not platitudes \n\n\
                 Be specific. Name the failure mode. This is a causal narrative, \
                 not an argument against the plan.",
        provider: "grok_hermes",
        model: "grok-4.3",
    },
];

/// Look up a direct-fire spec by wire slug. `None` for unknown modes.
pub fn spec(mode: &str) -> Option<&'static DirectFireSpec> {
    SPECS.iter().find(|s| s.slug == mode)
}

/// Prompt assembly — identical to the CLI direct-fire path in main.rs.
pub fn build_prompt(topic: &str, context: &str) -> String {
    if context.is_empty() {
        topic.to_string()
    } else {
        format!("{}\n\n---\n\n{}", context.trim(), topic)
    }
}

/// SessionMode tag for persisted direct-fire sessions (WS path).
pub fn session_mode(slug: &str) -> SessionMode {
    match slug {
        "contrarian" => SessionMode::Contrarian,
        "munger" => SessionMode::Munger,
        "kiss" => SessionMode::Kiss,
        "specops" => SessionMode::Specops,
        "premortem" => SessionMode::Premortem,
        _ => SessionMode::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pinned_modes_have_specs() {
        for slug in DIRECT_FIRE_MODES {
            let s = spec(slug).unwrap_or_else(|| panic!("missing spec for {slug}"));
            assert_eq!(s.slug, *slug);
            assert!(!s.display.is_empty());
            assert!(!s.system.is_empty());
            assert!(!s.provider.is_empty());
            assert!(!s.model.is_empty());
        }
        assert!(spec("wargame").is_none());
        assert!(spec("").is_none());
    }

    #[test]
    fn build_prompt_matches_cli_shape() {
        assert_eq!(build_prompt("Topic", ""), "Topic");
        assert_eq!(build_prompt("Topic", "ctx \n"), "ctx\n\n---\n\nTopic");
    }

    #[test]
    fn session_modes_serialize_to_wire_slugs() {
        for slug in DIRECT_FIRE_MODES {
            let mode = session_mode(slug);
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::json!(slug),
                "SessionMode for {slug} must serialize to its wire slug"
            );
        }
    }
}
