use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelState {
    pub tenant: String,
    pub sentinel: String,
    pub observed_at: i64, // unix epoch ms
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub state: SentinelState,
    pub reason: String,
    pub urgency: Urgency,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    Medium,
    High,
}
