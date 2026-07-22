//! War Room backend — REST endpoints beyond the core deliberation pipeline.
//!
//! Each submodule mirrors a Python file in `warroom/backend/`:
//!   - intervention_log: read sessions/intervention_log.jsonl + aggregate
//!   - lineage:          read sessions/lineage.jsonl + diff sessions
//!   - drift:            list/read runs/drift_*.md and runs/weekly_drift_*.json
//!   - mapmaker:         list/read runs/MAPMAKER_*.md
//!   - safe_map:         workspace-aware directory preview
//!   - embeddings:       semantic index via fastembed-rs MiniLM-L6-v2
//!   - fork:             cabinet fork from parent session
//!   - cabinets_save:    POST /api/cabinets/save validation + atomic write

pub mod cabinets_save;
pub mod clusters;
pub mod divergence;
pub mod drift;
pub mod embeddings;
pub mod fork;
pub mod intervention_log;
pub mod lineage;
pub mod mapmaker;
pub mod meta_review;
pub mod pdf;
pub mod predict;
pub mod safe_map;

use std::path::PathBuf;

/// Project root: env COUNCIL_SESSIONS_DIR's parent, else cwd.
pub fn project_root() -> PathBuf {
    if let Ok(s) = std::env::var("COUNCIL_SESSIONS_DIR") {
        let p = PathBuf::from(s);
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn sessions_dir() -> PathBuf {
    std::env::var("COUNCIL_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| project_root().join("sessions"))
}

pub fn runs_dir() -> PathBuf {
    std::env::var("COUNCIL_RUNS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| project_root().join("runs"))
}
