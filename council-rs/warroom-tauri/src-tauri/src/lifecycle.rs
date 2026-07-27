//! App-owned aggregate lifecycle/status contract.
//!
//! Council, optional Gateway Pack, and phone access each report a subsystem
//! lifecycle. Pure classification only — no process spawning here.

use crate::gateway_pack::GatewayPackState;
use crate::phone_access::PhoneAccessState;
use serde::{Deserialize, Serialize};

/// Shared subsystem lifecycle vocabulary for the desktop shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemLifecycle {
    Off,
    Starting,
    Ready,
    Degraded,
    Stopping,
    Error,
}

impl SubsystemLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
            Self::Error => "error",
        }
    }
}

/// Operator-facing aggregate product status (non-secret).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppLifecycleStatus {
    pub council: SubsystemLifecycle,
    pub gateway: SubsystemLifecycle,
    pub phone_access: SubsystemLifecycle,
    pub message: String,
}

/// Inputs for pure Council lifecycle classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CouncilLifecycleInput {
    pub owned_child: bool,
    pub stopping: bool,
    pub health_ready: bool,
    pub last_error: bool,
}

/// Map Council child + health into the shared lifecycle enum.
pub fn classify_council_lifecycle(input: CouncilLifecycleInput) -> SubsystemLifecycle {
    if input.stopping {
        return SubsystemLifecycle::Stopping;
    }
    if input.last_error && !input.health_ready {
        return SubsystemLifecycle::Error;
    }
    if input.owned_child {
        if input.health_ready {
            SubsystemLifecycle::Ready
        } else {
            SubsystemLifecycle::Starting
        }
    } else if input.health_ready {
        // Adopted external Council still counts as ready for the product surface.
        SubsystemLifecycle::Ready
    } else {
        SubsystemLifecycle::Off
    }
}

/// Map Gateway Pack status into the shared lifecycle enum.
pub fn classify_gateway_lifecycle(state: GatewayPackState) -> SubsystemLifecycle {
    match state {
        GatewayPackState::NotInstalled | GatewayPackState::Disabled => SubsystemLifecycle::Off,
        GatewayPackState::DockerMissing | GatewayPackState::DockerDaemonDown => {
            SubsystemLifecycle::Error
        }
        GatewayPackState::Installing | GatewayPackState::Starting => SubsystemLifecycle::Starting,
        GatewayPackState::InstalledStopped => SubsystemLifecycle::Off,
        GatewayPackState::AuthenticatedReady => SubsystemLifecycle::Ready,
        GatewayPackState::Degraded => SubsystemLifecycle::Degraded,
    }
}

/// Map phone-access status into the shared lifecycle enum.
pub fn classify_phone_lifecycle(state: PhoneAccessState) -> SubsystemLifecycle {
    match state {
        PhoneAccessState::Off => SubsystemLifecycle::Off,
        PhoneAccessState::Ready => SubsystemLifecycle::Ready,
        PhoneAccessState::PublishedButBackendDown => SubsystemLifecycle::Degraded,
        PhoneAccessState::TailscaleUnavailable
        | PhoneAccessState::NotLoggedIn
        | PhoneAccessState::ForeignUnowned
        | PhoneAccessState::FunnelPresent
        | PhoneAccessState::CommandError => SubsystemLifecycle::Error,
        PhoneAccessState::InterruptedChange => SubsystemLifecycle::Degraded,
        PhoneAccessState::Starting => SubsystemLifecycle::Starting,
        PhoneAccessState::Stopping => SubsystemLifecycle::Stopping,
    }
}

/// Compose the aggregate product status from subsystem classifications.
pub fn compose_app_lifecycle(
    council: SubsystemLifecycle,
    gateway: SubsystemLifecycle,
    phone_access: SubsystemLifecycle,
) -> AppLifecycleStatus {
    let message = match (council, gateway, phone_access) {
        (SubsystemLifecycle::Ready, SubsystemLifecycle::Off, SubsystemLifecycle::Off) => {
            "Council ready; Gateway and phone access off".to_string()
        }
        (SubsystemLifecycle::Ready, SubsystemLifecycle::Ready, SubsystemLifecycle::Ready) => {
            "Council, Gateway, and phone access ready".to_string()
        }
        (SubsystemLifecycle::Ready, _, SubsystemLifecycle::Ready) => {
            "Council and phone access ready".to_string()
        }
        (SubsystemLifecycle::Error, _, _) => "Council error".to_string(),
        (_, SubsystemLifecycle::Error, _) => "Gateway error".to_string(),
        (_, _, SubsystemLifecycle::Error) => "Phone access error".to_string(),
        (SubsystemLifecycle::Starting, _, _) => "Council starting".to_string(),
        (SubsystemLifecycle::Stopping, _, _) => "Council stopping".to_string(),
        (SubsystemLifecycle::Off, SubsystemLifecycle::Off, SubsystemLifecycle::Off) => {
            "All subsystems off".to_string()
        }
        _ => format!(
            "council={} gateway={} phone={}",
            council.as_str(),
            gateway.as_str(),
            phone_access.as_str()
        ),
    };
    AppLifecycleStatus {
        council,
        gateway,
        phone_access,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn council_lifecycle_transitions() {
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: false,
                stopping: false,
                health_ready: false,
                last_error: false,
            }),
            SubsystemLifecycle::Off
        );
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: true,
                stopping: false,
                health_ready: false,
                last_error: false,
            }),
            SubsystemLifecycle::Starting
        );
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: true,
                stopping: false,
                health_ready: true,
                last_error: false,
            }),
            SubsystemLifecycle::Ready
        );
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: true,
                stopping: true,
                health_ready: true,
                last_error: false,
            }),
            SubsystemLifecycle::Stopping
        );
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: false,
                stopping: false,
                health_ready: false,
                last_error: true,
            }),
            SubsystemLifecycle::Error
        );
        assert_eq!(
            classify_council_lifecycle(CouncilLifecycleInput {
                owned_child: false,
                stopping: false,
                health_ready: true,
                last_error: false,
            }),
            SubsystemLifecycle::Ready
        );
    }

    #[test]
    fn gateway_lifecycle_maps_pack_states() {
        assert_eq!(
            classify_gateway_lifecycle(GatewayPackState::NotInstalled),
            SubsystemLifecycle::Off
        );
        assert_eq!(
            classify_gateway_lifecycle(GatewayPackState::Starting),
            SubsystemLifecycle::Starting
        );
        assert_eq!(
            classify_gateway_lifecycle(GatewayPackState::AuthenticatedReady),
            SubsystemLifecycle::Ready
        );
        assert_eq!(
            classify_gateway_lifecycle(GatewayPackState::Degraded),
            SubsystemLifecycle::Degraded
        );
        assert_eq!(
            classify_gateway_lifecycle(GatewayPackState::DockerMissing),
            SubsystemLifecycle::Error
        );
    }

    #[test]
    fn phone_lifecycle_maps_publication_states() {
        assert_eq!(
            classify_phone_lifecycle(PhoneAccessState::Off),
            SubsystemLifecycle::Off
        );
        assert_eq!(
            classify_phone_lifecycle(PhoneAccessState::Ready),
            SubsystemLifecycle::Ready
        );
        assert_eq!(
            classify_phone_lifecycle(PhoneAccessState::PublishedButBackendDown),
            SubsystemLifecycle::Degraded
        );
        assert_eq!(
            classify_phone_lifecycle(PhoneAccessState::FunnelPresent),
            SubsystemLifecycle::Error
        );
        assert_eq!(
            classify_phone_lifecycle(PhoneAccessState::InterruptedChange),
            SubsystemLifecycle::Degraded
        );
    }

    #[test]
    fn compose_aggregate_message_is_deterministic() {
        let status = compose_app_lifecycle(
            SubsystemLifecycle::Ready,
            SubsystemLifecycle::Off,
            SubsystemLifecycle::Off,
        );
        assert_eq!(status.council, SubsystemLifecycle::Ready);
        assert!(status.message.contains("Council ready"));
    }
}
