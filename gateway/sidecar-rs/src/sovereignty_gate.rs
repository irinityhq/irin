// ==========================================================================
// sovereignty_gate.rs — Outbound values-alignment and safety gating.
//
// Port of Python love_equation_v2.py to Rust.
// Evaluates outbound AI responses against 5 core values and a risk profile.
// Enforces the strict >= 0.8 threshold for responses.
// ==========================================================================

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Configuration Constants
// ---------------------------------------------------------------------------

const BETA: f64 = 0.1;
const QUESTION_BOOST: f64 = 0.3;
const ALIGNMENT_MIN: f64 = 0.0;
const ALIGNMENT_MAX: f64 = 1.0;
const DEFAULT_RISK: f64 = 0.2;
const ENERGY_MIN: f64 = 1.0;
const KAPPA_MIN: f64 = 0.0;
const KAPPA_MAX: f64 = 1.0;
const SOVEREIGNTY_THRESHOLD: f64 = 0.8;

// ---------------------------------------------------------------------------
// Static Data
// ---------------------------------------------------------------------------

static CORE_VALUES: &[&str] = &[
    "Trust",
    "Acceptance",
    "Freedom",
    "Making a Difference",
    "Taking Care",
];

static VALUE_KEYWORDS: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert(
        "Trust",
        vec![
            "trust",
            "reliable",
            "honest",
            "transparent",
            "authentic",
            "integrity",
            "safe",
            "verifiable",
            "secure",
            "dependable",
        ],
    );
    m.insert(
        "Acceptance",
        vec![
            "accept",
            "embrace",
            "welcome",
            "allow",
            "compassion",
            "grace",
            "gentle",
            "kindness",
            "inclusive",
            "open",
            "non-judgmental",
        ],
    );
    m.insert(
        "Freedom",
        vec![
            "freedom",
            "choice",
            "autonomy",
            "sovereign",
            "liberate",
            "open",
            "explore",
            "autonomous",
            "independent",
            "self-determined",
        ],
    );
    m.insert(
        "Making a Difference",
        vec![
            "impact",
            "contribute",
            "meaningful",
            "purpose",
            "serve",
            "help",
            "improve",
            "transform",
            "benefit",
            "difference",
        ],
    );
    m.insert(
        "Taking Care",
        vec![
            "care", "nurture", "protect", "sustain", "maintain", "steward", "preserve", "safe",
            "support", "nourish",
        ],
    );
    m
});

static ACTION_RISK: LazyLock<HashMap<&'static str, f64>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("faiss_query", 0.05);
    m.insert("file_read", 0.05);
    m.insert("file_write", 0.3);
    m.insert("faiss_update", 0.3);
    m.insert("llm_call", 0.1);
    m.insert("synthesis", 0.1);
    m.insert("neo4j_write", 0.4);
    m.insert("mem0_write", 0.4);
    m.insert("api_call_external", 0.5);
    m.insert("telegram_send", 0.6);
    m.insert("message_send", 0.6);
    m.insert("email_send", 0.7);
    m.insert("trade_execute", 0.8);
    m
});

static QUESTION_WORDS: &[&str] = &[
    "what", "why", "how", "when", "where", "who", "which", "is", "are", "do", "does", "can",
    "could", "would", "should", "will", "shall", "may", "might",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SovereigntyScore {
    pub score: f64,           // The love derivative value (dE/dt)
    pub kappa: f64,           // Normalized [0,1]
    pub c_alignment: f64,     // Average value alignment (benefit)
    pub d_risk: f64,          // Risk assessment
    pub energy: f64,          // Input energy
    pub question_boost: bool, // Was question boost applied?
    pub allowed: bool,        // Enforced against SOVEREIGNTY_THRESHOLD
}

// ---------------------------------------------------------------------------
// Logic
// ---------------------------------------------------------------------------

pub struct SovereigntyGate {
    pub beta: f64,
    pub question_boost: f64,
}

impl Default for SovereigntyGate {
    fn default() -> Self {
        Self {
            beta: BETA,
            question_boost: QUESTION_BOOST,
        }
    }
}

impl SovereigntyGate {
    #[allow(dead_code)]
    pub fn new(beta: f64, question_boost: f64) -> Self {
        Self {
            beta,
            question_boost,
        }
    }

    fn align_to_value(&self, text: &str, value: &str) -> f64 {
        let keywords = match VALUE_KEYWORDS.get(value) {
            Some(kws) => kws,
            None => return ALIGNMENT_MIN,
        };

        if keywords.is_empty() {
            return ALIGNMENT_MIN;
        }

        let text_lower = text.to_lowercase();
        let mut hits = 0;
        for kw in keywords {
            if text_lower.contains(kw) {
                hits += 1;
            }
        }

        if hits == 0 {
            return ALIGNMENT_MIN;
        }

        let score = (hits as f64) / (keywords.len() as f64);
        score.min(ALIGNMENT_MAX)
    }

    fn is_question(&self, text: &str) -> bool {
        let stripped = text.trim();
        if stripped.is_empty() {
            return false;
        }

        if stripped.ends_with('?') {
            return true;
        }

        if let Some(first_word) = stripped.split_whitespace().next() {
            let fw_lower = first_word
                .to_lowercase()
                .trim_end_matches(&['?', '.', ',', ';', ':'][..])
                .to_string();

            if QUESTION_WORDS.contains(&fw_lower.as_str()) {
                return true;
            }
        }

        false
    }

    fn compute_risk(&self, action_type: &str) -> f64 {
        *ACTION_RISK.get(action_type).unwrap_or(&DEFAULT_RISK)
    }

    /// Compute Sovereignty Gate score (rule-based).
    ///
    /// Formula: dE/dt = beta * (C - D) * E
    /// - C = average alignment across 5 core values [0,1]
    /// - D = risk per action type [0,1]
    /// - E = energy (non-negative)
    pub fn evaluate(&self, action_desc: &str, action_type: &str, energy: f64) -> SovereigntyScore {
        let energy = energy.max(ENERGY_MIN);

        // 1. Compute C (alignment) -- average across 5 core values
        let mut sum_alignment = 0.0;
        for &val in CORE_VALUES {
            sum_alignment += self.align_to_value(action_desc, val);
        }
        let mut c_alignment = if !CORE_VALUES.is_empty() {
            sum_alignment / (CORE_VALUES.len() as f64)
        } else {
            0.0
        };

        // 2. Apply question boost if applicable
        let is_q = self.is_question(action_desc);
        if is_q {
            c_alignment = (c_alignment + self.question_boost).min(ALIGNMENT_MAX);
        }

        // 3. Compute D (risk)
        let d_risk = self.compute_risk(action_type);

        // 4. love_derivative = beta * (C - D) * energy
        let love_derivative = self.beta * (c_alignment - d_risk) * energy;

        // 5. kappa = clamp(love_derivative, 0.0, 1.0)
        let kappa = love_derivative.clamp(KAPPA_MIN, KAPPA_MAX);

        // 6. Gate check
        let allowed = kappa >= SOVEREIGNTY_THRESHOLD;

        SovereigntyScore {
            score: love_derivative,
            kappa,
            c_alignment,
            d_risk,
            energy,
            question_boost: is_q,
            allowed,
        }
    }
}
