//! Gateway Pack status types (operator-facing state machine).

use crate::docker_cli::{DESKTOP_COMPOSE_PROJECT, DESKTOP_GATEWAY_URL};
use serde::{Deserialize, Serialize};

/// Truthful operator-facing pack states. Never label a bare URL as ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPackState {
    NotInstalled,
    DockerMissing,
    DockerDaemonDown,
    Installing,
    InstalledStopped,
    Starting,
    AuthenticatedReady,
    Degraded,
    Disabled,
}

impl GatewayPackState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::DockerMissing => "docker_missing",
            Self::DockerDaemonDown => "docker_daemon_down",
            Self::Installing => "installing",
            Self::InstalledStopped => "installed_stopped",
            Self::Starting => "starting",
            Self::AuthenticatedReady => "authenticated_ready",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
        }
    }

    /// Governed proceedings may start only in this state.
    pub fn allows_governed(self) -> bool {
        matches!(self, Self::AuthenticatedReady)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayPackStatus {
    pub state: GatewayPackState,
    pub message: String,
    pub pack_version: Option<String>,
    pub manifest_mode: Option<String>,
    pub gateway_url: String,
    pub project: String,
    pub key_id: Option<String>,
    pub enabled: bool,
    pub docker: String,
    pub watch_producer_enabled: bool,
    pub watch_dispatcher_enabled: bool,
    pub authenticated: bool,
    /// True when Gateway is up, key authenticates, and Council is expected governed.
    pub council_governed: bool,
    /// Distinct from authenticated: URL field present / pack project known.
    pub gateway_url_configured: bool,
    pub support_matrix_summary: String,
    /// Pack enabled and live-authenticated — enough to spawn a governed child.
    /// Serialized so the renderer does not re-derive capability tiers.
    pub spawn_capable: bool,
    /// Full governed readiness (`AuthenticatedReady`): spawn_capable plus a
    /// proven owned governed Council child. Enroll/arm and the Deliberate toggle.
    pub governed_ready: bool,
    /// Structural hard-down for presentation demotion (disabled, Docker gap,
    /// not installed, stopped/not-running). Soft Degraded (auth flake, health
    /// flake, ungoverned child) is not hard-down.
    pub hard_down: bool,
}

impl GatewayPackStatus {
    pub(crate) fn base(state: GatewayPackState, message: impl Into<String>) -> Self {
        let mut st = Self {
            state,
            message: message.into(),
            pack_version: None,
            manifest_mode: None,
            gateway_url: DESKTOP_GATEWAY_URL.to_string(),
            project: DESKTOP_COMPOSE_PROJECT.to_string(),
            key_id: None,
            enabled: false,
            docker: "unknown".into(),
            watch_producer_enabled: false,
            watch_dispatcher_enabled: false,
            authenticated: false,
            council_governed: false,
            gateway_url_configured: true, // fixed loopback URL is always the pack target
            support_matrix_summary: SUPPORT_MATRIX_SUMMARY.to_string(),
            spawn_capable: false,
            governed_ready: false,
            hard_down: true,
        };
        st.refresh_predicates(false);
        st
    }

    /// Recompute the canonical capability predicates after mutating state fields.
    ///
    /// `pack_not_running` is true when the owned compose project is known not
    /// running (enabled-but-stopped is `Degraded` in the ladder, but still
    /// hard-down so sticky presentation cannot claim ready).
    pub fn refresh_predicates(&mut self, pack_not_running: bool) {
        self.spawn_capable = self.enabled && self.authenticated;
        // Full governed readiness requires pack auth and a proven owned child;
        // an AuthenticatedReady-shaped value alone is not authority.
        self.governed_ready =
            self.spawn_capable && self.council_governed && self.state.allows_governed();
        self.hard_down = Self::classify_hard_down(self.enabled, self.state, pack_not_running);
    }

    /// Exhaustive hard-down classifier over [`GatewayPackState`].
    ///
    /// Soft failures (auth/health flake, ungoverned child) stay `Degraded` with
    /// `pack_not_running=false` and are not hard-down. Stopped containers on an
    /// enabled pack also land as `Degraded` but pass `pack_not_running=true`.
    pub fn classify_hard_down(
        enabled: bool,
        state: GatewayPackState,
        pack_not_running: bool,
    ) -> bool {
        if !enabled || pack_not_running {
            return true;
        }
        match state {
            GatewayPackState::DockerMissing
            | GatewayPackState::DockerDaemonDown
            | GatewayPackState::NotInstalled
            | GatewayPackState::InstalledStopped
            | GatewayPackState::Disabled => true,
            GatewayPackState::Installing
            | GatewayPackState::Starting
            | GatewayPackState::AuthenticatedReady
            | GatewayPackState::Degraded => false,
        }
    }
}

pub const SUPPORT_MATRIX_SUMMARY: &str = "\
v0.1: API-key providers (xAI/OpenAI/Anthropic/NVIDIA) when present in login env; \
Vertex Direct-only (no gcloud mount); Claude/Codex CLI proxies supported when operator CLIs are installed and authenticated; \
Watch producer/dispatcher forced off.";
