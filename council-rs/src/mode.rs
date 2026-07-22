//! Deliberation Mode — TearDown, Pathfind, Harden.
//!
//! The toggle that changes HOW the council thinks, not WHO's in it.
//! Orthogonal to cabinet choice (--cabinet warroom --harden works).

use serde::{Deserialize, Serialize};

/// The deliberation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// "Kill it if it deserves killing." Default council behavior.
    /// Objections encouraged. Dead-end output acceptable.
    #[default]
    TearDown,

    /// "Don't stop til you find a way." No dead-end output.
    /// Objections must be paired with paths/fallbacks/scope-cuts.
    Pathfind,

    /// "Stress like a redteam, harden like a builder." Adversarial analysis
    /// PLUS a paired-replacement constraint: every flaw, kill, or rejection
    /// must come with the better way (drawn from prior art when possible,
    /// or designed from first principles). Outputs ratify, ratify-with-
    /// changes, or replace-with-design — never bare "this is broken."
    Harden,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::TearDown => write!(f, "tear-down"),
            Mode::Pathfind => write!(f, "pathfind"),
            Mode::Harden => write!(f, "harden"),
        }
    }
}

/// Preamble injected AFTER _RESTATE_GATE but BEFORE the topic, per mode.
impl Mode {
    pub fn seat_preamble(&self) -> &'static str {
        match self {
            Mode::TearDown => TEARDOWN_PREAMBLE,
            Mode::Pathfind => PATHFIND_PREAMBLE,
            Mode::Harden => HARDEN_PREAMBLE,
        }
    }

    pub fn chair_instruction(&self) -> &'static str {
        match self {
            Mode::TearDown => TEARDOWN_CHAIR,
            Mode::Pathfind => PATHFIND_CHAIR,
            Mode::Harden => HARDEN_CHAIR,
        }
    }

    /// Convergence threshold for this mode.
    /// Pathfind can diverge more (multiple valid paths = good output).
    /// Harden sits between: convergence on the verdict matters, but
    /// divergent improvement proposals are signal, not noise.
    pub fn convergence_threshold(&self) -> f64 {
        match self {
            Mode::TearDown => 0.8,
            Mode::Pathfind => 0.6, // Divergent paths are OK
            Mode::Harden => 0.7,
        }
    }

    /// Should SpecOps fire in this mode?
    /// Pathfind: only if paths are contradictory, not merely different.
    /// Harden: between — fire if seats disagree on whether the work
    /// ratifies vs needs replacement, not merely on which improvements.
    pub fn specops_threshold(&self) -> f64 {
        match self {
            Mode::TearDown => 0.8,
            Mode::Pathfind => 0.4, // Only fire on true incoherence
            Mode::Harden => 0.6,
        }
    }
}

// ---------------------------------------------------------------------------
// Mode Prompts — The soul of each mode
// ---------------------------------------------------------------------------

const TEARDOWN_PREAMBLE: &str = "\
DELIBERATION MODE: TEAR IT DOWN

Your role in this deliberation is adversarial analysis. You are here to find every \
flaw, every risk, every reason this should NOT proceed. Be ruthless but honest.

Rules:
- Dead-end conclusions are acceptable: if this idea should die, say so and say why.
- Enumerate failure modes with severity tags (P0-critical, P1-high, P2-medium).
- Attack assumptions — especially the ones that feel obvious.
- If you see a path forward, state it, but don't strain to find one.
- The best outcome is an idea that survives your assault — or one that dies before \
  it costs real money.";

const PATHFIND_PREAMBLE: &str = "\
DELIBERATION MODE: PATHFINDER — DON'T STOP TIL YOU FIND A WAY

Your role in this deliberation is constructive problem-solving. Dead-end output is \
FORBIDDEN. Every problem must be converted into a path forward.

Rules:
- You MUST produce at least one concrete, actionable path forward. No exceptions.
- Objections are WELCOME — but every objection must be paired with a solution, \
  fallback, scope-cut, or 'nearest achievable version with the tradeoff named.'
- 'This won't work' is not acceptable output. 'This won't work AS STATED, but \
  here's what WILL work if we...' IS acceptable.
- If the obvious path is blocked, find the non-obvious one. If that's blocked, \
  invent a new one. If invention fails, scope-cut to what IS possible and name the \
  tradeoff explicitly.
- The ONLY absolute exception: if the goal is physically impossible, illegal, or \
  poses clear safety risk. In that case, state the constraint, then describe the \
  nearest achievable version.
- Rank your paths: Path A (preferred), Path B (fallback), Path C (minimal viable).
- Include estimated effort, cost, and confidence for each path.
- The best outcome is a clear, executable plan that the team can start on Monday.";

const TEARDOWN_CHAIR: &str = "\
You are the Chair in TEAR-DOWN mode. Your synthesis should identify which objections \
are real vs performative, which risks are tolerable, and whether the idea survives or \
should be killed. Own the decision — don't hedge.";

const PATHFIND_CHAIR: &str = "\
You are the Chair in PATHFINDER mode. Your synthesis should consolidate the proposed \
paths into a ranked action plan. Where seats found different paths, evaluate which is \
strongest by feasibility, cost, and time. Where seats found the same path, treat that \
as HIGH CONFIDENCE. Produce a concrete, ordered action list — not a summary of opinions. \
If any seat violated the 'no dead-end' rule, note it and provide the path they should \
have found. Tag any unvalidated paths that should NOT be stored as precedent.";

const HARDEN_PREAMBLE: &str = "\
DELIBERATION MODE: HARDEN — STRESS LIKE A REDTEAM, BUILD LIKE A CRAFTSMAN

Your role in this deliberation is constructive adversarial analysis. You will \
stress-test the work as ruthlessly as a tear-down review — and for every flaw \
you surface, you will pair it with the better way to do it. No bare 'this is \
broken' verdicts. No dead-end objections. The output is a hardening plan, not \
a kill list.

Rules:
- Open with the GEMS. Before listing flaws, name what is worth preserving — the \
  patterns, decisions, or invariants in this work that should be elevated, copied \
  elsewhere, or stored as precedent. The reviewer who can only see what is broken \
  is half a reviewer.
- Stress-test ruthlessly. Find every flaw, every risk, every silent failure mode. \
  Tag with severity: P0 (must close before merge), P1 (close within sprint), P2 \
  (defer with explicit owner). Be specific — file:line where possible.
- EVERY P0/P1 must be paired with the better way. The pairing is mandatory, not \
  decorative. Acceptable forms:
    (a) Cite a known-better pattern from prior art (with name + source).
    (b) Design the replacement from first principles in concrete enough detail \
        that an engineer could start implementing it.
    (c) Scope-cut to the nearest achievable version with the tradeoff named.
- 'This is wrong' alone is incomplete output. 'This is wrong; here is the \
  proven pattern X from system Y; apply it like this:' is complete.
- Look for non-obvious improvements the work itself didn't request — gems the \
  builder may have missed. If the work picks pattern A and pattern B would be \
  cleaner, propose B with the case for switching.
- Verdicts available — pick exactly one and own it:
    RATIFY                 — ship as-is; gems noted; no P0s/P1s found.
    RATIFY WITH CHANGES    — ship after these P0s close; each paired with the fix.
    REPLACE WITH DESIGN    — fundamentally wrong; here is the better design, \
                             not just the objection.
    BLOCK PENDING DECISION — only if the better way requires a human policy \
                             choice you cannot make for them; state the choice.
- A 'BLOCK' verdict without an articulated alternative path is a failed review. \
  If you cannot find a better way, do the research before declaring the verdict — \
  cite what you searched for and why nothing fits. Then propose what you would \
  build if forced to choose.
- The best outcome is work that emerges harder than it went in: gems preserved, \
  flaws closed with proven patterns, the next iteration teed up.";

const HARDEN_CHAIR: &str = "\
You are the Chair in HARDEN mode. Your synthesis should produce a hardening \
plan, not a verdict-only summary. Structure it: \
(1) Gems — what every seat agreed is worth preserving / promoting to precedent. \
(2) Verdict — exactly one of: RATIFY, RATIFY WITH CHANGES, REPLACE WITH DESIGN, \
    BLOCK PENDING DECISION (only if a paired alternative requires a human \
    policy choice). Own the call. \
(3) Hardening actions — for each P0/P1 surfaced by any seat, the action \
    paired with the better way (named pattern, file:line target, effort). \
    Strip any objection no seat could pair with a fix; flag those as 'seat \
    failed the harden constraint' rather than passing them through. \
(4) Better-way proposals beyond the requested scope — gems the builder \
    didn't ask for but would benefit from. Tag confidence and effort. \
If seats converged on the same flaw with the same fix, treat as HIGH \
CONFIDENCE. If seats diverged on the better way, name the tradeoffs and \
recommend one — do not punt to the reader. Tag what is precedent-worthy and \
what should NOT be stored until validated in practice.";
