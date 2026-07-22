//! Intervention queue — async channel for client→server actions during pause.
//!
//! Maps directly to Python's InterventionQueue class.
//! Used during `awaiting_input` to receive operator decisions.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

/// Client intervention actions — mirrors council_stream.py protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Intervention {
    Continue,
    EndEarly,
    EscalateSpecops,
    EscalateMunger,
    EscalateContrarian,
    EscalateKiss,
    InjectContext {
        #[serde(default)]
        text: String,
    },
    SwapSeat {
        seat_name: String,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        system: Option<String>,
    },
}

impl Intervention {
    /// Parse from a generic JSON value (from WebSocket message).
    pub fn from_value(v: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(v.clone()).ok()
    }

    pub fn action_name(&self) -> &str {
        match self {
            Self::Continue => "continue",
            Self::EndEarly => "end_early",
            Self::EscalateSpecops => "escalate_specops",
            Self::EscalateMunger => "escalate_munger",
            Self::EscalateContrarian => "escalate_contrarian",
            Self::EscalateKiss => "escalate_kiss",
            Self::InjectContext { .. } => "inject_context",
            Self::SwapSeat { .. } => "swap_seat",
        }
    }

    pub fn is_escalation(&self) -> bool {
        matches!(
            self,
            Self::EscalateSpecops
                | Self::EscalateMunger
                | Self::EscalateContrarian
                | Self::EscalateKiss
        )
    }

    pub fn escalation_mode(&self) -> Option<&str> {
        match self {
            Self::EscalateSpecops => Some("specops"),
            Self::EscalateMunger => Some("munger"),
            Self::EscalateContrarian => Some("contrarian"),
            Self::EscalateKiss => Some("kiss"),
            _ => None,
        }
    }
}

/// Async-safe intervention queue.
/// Producer: WebSocket intake loop (client messages)
/// Consumer: Streaming deliberation loop (pause points)
pub struct InterventionQueue {
    tx: mpsc::Sender<Intervention>,
    rx: mpsc::Receiver<Intervention>,
}

impl Default for InterventionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InterventionQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(16);
        Self { tx, rx }
    }

    /// Get a sender handle (cloneable, for the intake loop).
    pub fn sender(&self) -> mpsc::Sender<Intervention> {
        self.tx.clone()
    }

    /// Push an intervention from the client side.
    pub async fn push(&self, action: Intervention) {
        let _ = self.tx.send(action).await;
    }

    /// Wait for the next intervention, with timeout.
    /// Returns Continue on timeout (matches Python behavior).
    pub async fn wait(&mut self, timeout_secs: u64) -> Intervention {
        match timeout(Duration::from_secs(timeout_secs), self.rx.recv()).await {
            Ok(Some(action)) => action,
            Ok(None) => Intervention::Continue, // Channel closed
            Err(_) => Intervention::Continue,   // Timeout
        }
    }
}
