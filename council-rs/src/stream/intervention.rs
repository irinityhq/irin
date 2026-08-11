//! Intervention queue — async channel for client→server actions during pause.
//!
//! Maps directly to Python's InterventionQueue class.
//! Used during `awaiting_input` to receive operator decisions.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

/// Client intervention actions — mirrors council_stream.py protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant;

    #[tokio::test]
    async fn wait_timeout_defaults_to_continue() {
        let mut q = InterventionQueue::new();
        let started = Instant::now();
        let action = q.wait(1).await;
        assert!(
            matches!(action, Intervention::Continue),
            "timeout must yield Continue, got {action:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "must actually wait ~timeout_secs"
        );
    }

    #[tokio::test]
    async fn wait_returns_pushed_action() {
        let mut q = InterventionQueue::new();
        let tx = q.sender();
        tokio::spawn(async move {
            tx.send(Intervention::EndEarly).await.unwrap();
        });
        let action = q.wait(5).await;
        assert!(
            matches!(action, Intervention::EndEarly),
            "must surface EndEarly, got {action:?}"
        );
    }

    #[tokio::test]
    async fn closed_channel_defaults_to_continue() {
        // Defensive-branch pin: `new()` keeps a live sender inside the queue,
        // so recv never observes a closed channel through the public
        // constructor. Construct the state directly to characterize the
        // `Ok(None) => Continue` fallback (Python parity).
        let (tx, rx) = mpsc::channel::<Intervention>(1);
        drop(tx);
        let (spare_tx, _spare_rx) = mpsc::channel(1);
        let mut closed = InterventionQueue { tx: spare_tx, rx };
        let action = closed.wait(5).await;
        assert_eq!(action, Intervention::Continue);
    }

    #[test]
    fn action_name_and_escalation_helpers() {
        assert_eq!(Intervention::Continue.action_name(), "continue");
        assert_eq!(Intervention::EndEarly.action_name(), "end_early");
        assert!(!Intervention::Continue.is_escalation());
        assert!(Intervention::EscalateSpecops.is_escalation());
        assert_eq!(
            Intervention::EscalateMunger.escalation_mode(),
            Some("munger")
        );
        assert_eq!(
            Intervention::from_value(&serde_json::json!({"action": "continue"})),
            Some(Intervention::Continue)
        );
    }
}
