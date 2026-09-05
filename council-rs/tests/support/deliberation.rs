use council_rs::types::{RoleCascadeStep, RoleDefinition, RolesConfig, Seat};
use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};

/// Serialize tests that mutate process env (sessions dir, evidence switches).
pub async fn env_lock() -> MutexGuard<'static, ()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(())).lock().await
}

fn mock_step(model: &str) -> RoleCascadeStep {
    RoleCascadeStep {
        provider: "mock".into(),
        model: model.into(),
        max_tokens: 256,
    }
}

pub fn mock_roles() -> RolesConfig {
    let step = mock_step("mock-role");
    let validator = mock_step("mock-claim-validator");
    RolesConfig {
        convergence_judge: RoleDefinition {
            description: "test judge".into(),
            cascade: vec![step.clone()],
        },
        frame_check: RoleDefinition {
            description: "test frame".into(),
            cascade: vec![step.clone()],
        },
        claim_validator: RoleDefinition {
            description: "test validator".into(),
            cascade: vec![validator],
        },
        scope_auditor: RoleDefinition {
            description: "test auditor".into(),
            cascade: vec![step],
        },
    }
}

pub fn mock_seat(name: &str, model: &str) -> Seat {
    Seat {
        name: name.into(),
        provider: "mock".into(),
        model: model.into(),
        system: "You are a mock seat.".into(),
    }
}
