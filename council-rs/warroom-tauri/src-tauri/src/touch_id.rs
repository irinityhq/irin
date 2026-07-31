//! Touch ID as a product control — native bridge to the existing dual-custody
//! local-attest arm ceremony.
//!
//! Privileged native boundary. This module owns:
//!
//! * the bundled `arm-attest` helper (Secure Enclave keypair, `.biometryCurrentSet`
//!   Touch ID gate, ES256 signing) — invoked only from here, never from the renderer;
//! * the app-owned enrollment registry (PUBLIC credential records only) and the
//!   Keychain-held arm-principal bearer token (custody domain 1);
//! * the existing `stage` / `pending` / `confirm` / `disarm` protocol against the
//!   app-owned Gateway. No parallel arming system is introduced: every request
//!   goes to the same sidecar routes `gateway/bin/arm` uses.
//!
//! What crosses to the renderer is [`TouchIdStatus`]: an enum, booleans, counts,
//! and a lease deadline. Never a private key, wrapped key blob, admin token,
//! Gateway credential, principal token, challenge, attestation, signature, or
//! registry contents.
//!
//! Default-off is preserved end to end. Enabling Gateway does not enroll;
//! enrolling does not arm; arming expires; disarm is immediate; nothing here runs
//! at launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::docker_cli::{run_command_timeout, DESKTOP_GATEWAY_URL};
#[cfg(test)]
use crate::gateway_pack::ARM_KEYS_CONTAINER_PATH;
use crate::gateway_pack::{arm_keys_path, gateway_data_dir};
use crate::keychain::{
    is_valid_arm_principal_token, load_arm_principal_token, resolve_arm_principal,
    store_arm_principal_token, ArmPrincipalProbeMode, SecretStore, ARM_PRINCIPAL_NAME,
};
use crate::paths::executable_dir;

/// Bundled helper name under the app's standard `Contents/Helpers` code
/// directory. Shipped inside the app bundle so it is covered by the app's
/// Developer ID signature and notarization ticket; its on-disk digest is the
/// code identity pinned at enrollment.
pub const HELPER_BIN_NAME: &str = "arm-attest";

/// App-owned enrollment record (never the key blob itself).
const ENROLLMENT_RECORD_NAME: &str = "touch-id-enrollment.json";

/// Installed-product helper state. The helper's legacy CLI default remains
/// `~/.config/gateway`; IRIN.app always overrides it to this app-owned
/// Application Support directory so an old ad-hoc blob is never silently
/// adopted.
const ARM_ATTEST_DIR_NAME: &str = "touch-id-attest";
const ENCLAVE_KEY_NAME: &str = "arm-attest.key";

/// Bounded helper runtime. A Touch ID prompt the operator ignores must not wedge
/// the shell; the ceremony fails closed and the stage expires on its own.
const HELPER_TIMEOUT: Duration = Duration::from_secs(75);

/// Bounded ceremony HTTP timeout against the loopback Gateway.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Process-wide ordering fence for Touch ID arm/renew versus Disarm.
///
/// A ceremony deliberately does not hold a mutex while the biometric prompt is
/// open: Disarm must remain an immediate kill switch. Instead, each ceremony
/// carries the current generation and re-validates it under a short boundary
/// lock immediately before it may create/resume a stage and immediately before
/// it may confirm. Disarm advances the generation before taking that boundary,
/// so an already-waiting helper result can never confirm after the kill switch.
struct CeremonyFence {
    state: Mutex<CeremonyFenceState>,
    boundary: Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CeremonyTicket(u64);

#[derive(Debug)]
struct CeremonyFenceState {
    generation: u64,
    disarm_in_progress: bool,
}

impl CeremonyFence {
    const fn new() -> Self {
        Self {
            state: Mutex::new(CeremonyFenceState {
                generation: 0,
                disarm_in_progress: false,
            }),
            boundary: Mutex::new(()),
        }
    }

    fn begin_ceremony(&self) -> Result<CeremonyTicket, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "Touch ID ceremony fence is unavailable".to_string())?;
        if state.disarm_in_progress {
            return Err("Touch ID disarm is in progress; retry after it completes".to_string());
        }
        Ok(CeremonyTicket(state.generation))
    }

    fn ensure_current(&self, ticket: CeremonyTicket) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "Touch ID ceremony fence is unavailable".to_string())?;
        if state.disarm_in_progress || state.generation != ticket.0 {
            return Err(
                "Touch ID ceremony was cancelled by Disarm; the stale stage was not confirmed"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Run one stage/confirm boundary only while `ticket` is still current.
    ///
    /// Disarm advances the generation before waiting for this lock. If it races
    /// an already-running boundary, its server-side disarm runs immediately
    /// afterward and clears the stage/lease that boundary could have created.
    fn run_if_current<T>(
        &self,
        ticket: CeremonyTicket,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _boundary = self
            .boundary
            .lock()
            .map_err(|_| "Touch ID ceremony boundary is unavailable".to_string())?;
        self.ensure_current(ticket)?;
        operation()
    }

    /// Invalidate every outstanding ceremony and run the server-side kill
    /// switch. New ceremonies are refused until the disarm request returns.
    fn cancel_and_run<T>(
        &self,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Touch ID ceremony fence is unavailable".to_string())?;
            if state.disarm_in_progress {
                return Err("Touch ID disarm is already in progress".to_string());
            }
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or_else(|| "Touch ID ceremony generation exhausted".to_string())?;
            state.disarm_in_progress = true;
        }

        let boundary = self
            .boundary
            .lock()
            .map_err(|_| "Touch ID ceremony boundary is unavailable".to_string());
        let result = match boundary {
            Ok(_boundary) => operation(),
            Err(error) => Err(error),
        };

        let finish = self
            .state
            .lock()
            .map_err(|_| "Touch ID ceremony fence is unavailable".to_string())
            .map(|mut state| {
                state.disarm_in_progress = false;
            });
        finish?;
        result
    }
}

static TOUCH_ID_CEREMONY_FENCE: CeremonyFence = CeremonyFence::new();

/// Sticky presentation flag: the most recent successful ceremony returned
/// `rehearsal-ok` (producer did not start). Cleared on real arm, disarm,
/// enroll, or pack lifecycle change. The panel must render a distinct state
/// ("Rehearsal passed — not armed") rather than never-armed "Touch ID ready".
static REHEARSAL_PASSED_STICKY: AtomicBool = AtomicBool::new(false);

/// Record that the last ceremony completed as rehearsal-only.
pub fn note_rehearsal_ok() {
    REHEARSAL_PASSED_STICKY.store(true, Ordering::SeqCst);
}

/// Clear the post-rehearsal presentation sticky.
pub fn clear_rehearsal_passed() {
    REHEARSAL_PASSED_STICKY.store(false, Ordering::SeqCst);
}

fn rehearsal_passed_sticky() -> bool {
    REHEARSAL_PASSED_STICKY.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Renderer-facing projection
// ---------------------------------------------------------------------------

/// Product states for the control that sits beside Gateway in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchIdState {
    /// This build/host cannot run the ceremony at all (helper absent).
    Unavailable,
    /// A prerequisite outside Touch ID is missing (Gateway / Watch surface).
    Blocked,
    /// No enrollment yet — "Set up Touch ID".
    SetupRequired,
    /// Enrollment exists but is not trustworthy — "Re-enroll Touch ID".
    ReenrollRequired,
    /// Enrolled and disarmed — "Touch ID ready" / "Arm with Touch ID".
    Ready,
    /// A stage is open; a tap is expected.
    CeremonyOpen,
    /// Armed under an expiring, audited lease — "Armed until <time>".
    Armed,
}

/// Why a control is disabled or degraded. Stable machine tags — the renderer
/// maps them to copy; they never carry a path, token, or identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchIdReason {
    HelperMissing,
    GatewayNotReady,
    WatchSurfaceUnreachable,
    ArmPrincipalMissing,
    RegistryUnloaded,
    RegistryMismatch,
    HelperIdentityChanged,
    EnclaveKeyMissing,
    EnrollmentMissing,
    RehearsalOnlyBuild,
    LeaseExpired,
}

/// The complete renderer-visible Touch ID projection. Every field is a
/// non-secret derivation; see the module docs for what may never appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchIdStatus {
    pub state: TouchIdState,
    /// Machine-stable reason tag when the control is not fully usable.
    pub reason: Option<TouchIdReason>,
    /// Lease deadline in Unix milliseconds — the "Armed until <time>" source.
    pub armed_exp_at_ms: Option<i64>,
    /// Remaining lease, clamped at 0.
    pub armed_expires_in_ms: Option<u64>,
    /// Remaining stage window while a ceremony is open.
    pub stage_expires_in_ms: Option<u64>,
    /// Enrollment record present AND consistent with the running sidecar.
    pub enrolled: bool,
    /// This build may arm the real producer (a dirty build is rehearsal-only).
    pub allow_real_arm: bool,
    /// Enabled-action projection — the renderer never re-derives these.
    pub can_enroll: bool,
    pub can_arm: bool,
    pub can_renew: bool,
    pub can_disarm: bool,
    /// Last successful ceremony was `rehearsal-ok` — panel must not look
    /// identical to never-armed "Touch ID ready".
    pub rehearsal_passed: bool,
}

impl TouchIdStatus {
    fn of(state: TouchIdState, reason: Option<TouchIdReason>) -> Self {
        Self {
            state,
            reason,
            armed_exp_at_ms: None,
            armed_expires_in_ms: None,
            stage_expires_in_ms: None,
            enrolled: false,
            allow_real_arm: false,
            can_enroll: false,
            can_arm: false,
            can_renew: false,
            can_disarm: false,
            rehearsal_passed: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure state derivation (unit-tested; no Touch ID, Docker, or network)
// ---------------------------------------------------------------------------

/// Everything the state machine is allowed to look at. Gathering these is IO;
/// deciding from them is not, so the decision is pure and fully testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TouchIdInputs {
    /// The bundled helper exists and is executable.
    pub helper_present: bool,
    /// The helper's on-disk digest matches the enrollment record.
    pub helper_identity_matches: bool,
    /// The Secure Enclave wrapped-key blob exists (existence only).
    pub enclave_key_present: bool,
    /// An app-owned enrollment record was read successfully.
    pub enrollment_record_present: bool,
    /// The app-owned registry still hashes to the enrollment record.
    pub local_registry_matches_enrollment: bool,
    /// The Gateway Pack is authenticated and running.
    pub gateway_ready: bool,
    /// The arm-principal bearer token is in the Keychain.
    pub arm_principal_present: bool,
    /// The sidecar answered the status route.
    pub watch_reachable: bool,
    /// A background tick intentionally skipped the Watch request because the
    /// lifecycle presence cache held no bearer. This is not a reachability
    /// failure; status authority may retain its last authenticated projection.
    pub watch_probe_deferred: bool,
    /// The sidecar loaded a usable enrollment registry.
    pub registry_loaded: bool,
    /// The sidecar's keyset digest equals the digest of OUR registry file.
    pub registry_matches_enrollment: bool,
    /// At least one arm principal is configured on the sidecar.
    pub arm_capable: bool,
    /// The sidecar build may arm for real (else every ceremony is a rehearsal).
    pub allow_real_arm: bool,
    /// A producer is armed right now.
    pub armed: bool,
    pub armed_exp_at_ms: Option<i64>,
    pub armed_expires_in_ms: Option<u64>,
    /// A stage is open and unexpired.
    pub ceremony_open: bool,
    pub stage_expires_in_ms: Option<u64>,
}

fn effective_real_arm_permission(sidecar_allows: bool, host_is_dirty: bool) -> bool {
    sidecar_allows && !host_is_dirty
}

fn host_requires_rehearsal() -> bool {
    crate::bundled_build_identity().1
}

/// Sidecar stage body uses the wire key `rehearse` (not `rehearsal`).
/// See `gateway/sidecar-rs/src/watch/api/arming.rs` admin_arm_stage_json.
fn stage_request_body(host_rehearsal: bool) -> Option<serde_json::Value> {
    host_rehearsal.then(|| serde_json::json!({ "rehearse": true }))
}

/// How a clean/dirty host treats a pending stage found at GET arm/pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingStageDisposition {
    /// Resume the open stage (same challenge/nonce).
    Resume,
    /// Dirty host must not resume a real-arm stage — refuse loudly.
    RefuseRealStageOnDirtyHost,
    /// Clean real-capable host must not resume a rehearsal stage — POST fresh.
    StageFresh,
}

fn classify_pending_stage(host_rehearsal: bool, stage_rehearsal: bool) -> PendingStageDisposition {
    if host_rehearsal && !stage_rehearsal {
        PendingStageDisposition::RefuseRealStageOnDirtyHost
    } else if !host_rehearsal && stage_rehearsal {
        PendingStageDisposition::StageFresh
    } else {
        PendingStageDisposition::Resume
    }
}

/// Presentation hysteresis for Gateway readiness on the status path only.
///
/// Background Touch ID polls re-run a multi-step Docker + HTTP pack probe. A
/// single soft failure (Degraded / auth timeout) must not grey the Re-enroll
/// button until the next sample. Hard-down states (disabled, Docker missing,
/// stopped) demote immediately so a deliberate Disable is not sticky.
///
/// Action paths (enroll/arm) must never use this — they fail closed on a
/// fresh sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GatewayReadySticky {
    last_true_at_ms: Option<i64>,
}

impl GatewayReadySticky {
    /// Default hold covers two 8s Settings polls so a transient soft failure
    /// cannot flip the primary control between ticks.
    pub const DEFAULT_HOLD_MS: i64 = 20_000;

    pub const fn new() -> Self {
        Self {
            last_true_at_ms: None,
        }
    }

    pub fn project(
        &mut self,
        sample_ready: bool,
        hard_down: bool,
        now_ms: i64,
        hold_ms: i64,
    ) -> bool {
        if sample_ready {
            self.last_true_at_ms = Some(now_ms);
            return true;
        }
        if hard_down {
            self.last_true_at_ms = None;
            return false;
        }
        match self.last_true_at_ms {
            Some(t) if now_ms.saturating_sub(t) <= hold_ms => true,
            _ => {
                self.last_true_at_ms = None;
                false
            }
        }
    }
}

/// The product state machine.
///
/// Order matters and is deliberately fail-closed: capability first (no helper →
/// nothing is offered), then prerequisites, then enrollment trust, then the
/// live lease. A degraded input never resolves to a more permissive state than
/// a healthy one.
pub fn derive_status(inp: &TouchIdInputs) -> TouchIdStatus {
    // 1. Capability. Without the bundled helper there is no ceremony to offer.
    if !inp.helper_present {
        return TouchIdStatus::of(
            TouchIdState::Unavailable,
            Some(TouchIdReason::HelperMissing),
        );
    }

    // 2. Enrollment trust is evaluated before the live Watch prerequisites so
    //    the operator can see setup/re-enrollment state while Gateway is down.
    //    The action itself stays disabled until Gateway is ready, matching the
    //    native enrollment command. Neither state exposes an arming action.
    //
    //    "Old ad-hoc helper" case: an enclave blob with no app-owned record is
    //    NOT an enrollment. There is no deterministic proof of code-identity or
    //    enclave continuity for it, so it demands explicit re-enrollment rather
    //    than an attempt to reuse the wrapped blob.
    if !inp.enrollment_record_present {
        let (state, reason) = if inp.enclave_key_present {
            (
                TouchIdState::ReenrollRequired,
                TouchIdReason::EnrollmentMissing,
            )
        } else {
            (
                TouchIdState::SetupRequired,
                TouchIdReason::EnrollmentMissing,
            )
        };
        let mut st = TouchIdStatus::of(state, Some(reason));
        st.allow_real_arm = inp.allow_real_arm;
        st.can_enroll = inp.gateway_ready;
        return st;
    }
    if !inp.helper_identity_matches {
        let mut st = TouchIdStatus::of(
            TouchIdState::ReenrollRequired,
            Some(TouchIdReason::HelperIdentityChanged),
        );
        st.can_enroll = inp.gateway_ready;
        return st;
    }
    if !inp.enclave_key_present {
        let mut st = TouchIdStatus::of(
            TouchIdState::ReenrollRequired,
            Some(TouchIdReason::EnclaveKeyMissing),
        );
        st.can_enroll = inp.gateway_ready;
        return st;
    }
    if !inp.local_registry_matches_enrollment {
        let mut st = TouchIdStatus::of(
            TouchIdState::ReenrollRequired,
            Some(TouchIdReason::RegistryMismatch),
        );
        st.can_enroll = inp.gateway_ready;
        return st;
    }

    // 3. Prerequisites. Precise, distinguishable reasons — "Gateway is off" and
    //    "the Watch surface did not answer" are different operator problems.
    if !inp.gateway_ready {
        let mut st = TouchIdStatus::of(TouchIdState::Blocked, Some(TouchIdReason::GatewayNotReady));
        st.enrolled = true;
        return st;
    }
    if !inp.watch_reachable {
        let mut st = TouchIdStatus::of(
            TouchIdState::Blocked,
            Some(TouchIdReason::WatchSurfaceUnreachable),
        );
        st.enrolled = true;
        return st;
    }
    if !inp.arm_principal_present || !inp.arm_capable {
        let mut st = TouchIdStatus::of(
            TouchIdState::Blocked,
            Some(TouchIdReason::ArmPrincipalMissing),
        );
        st.enrolled = true;
        return st;
    }

    // 4. Registry continuity against the RUNNING sidecar. A pack restart that
    //    did not load our registry, or loaded a different one, is a re-enroll —
    //    never a silent downgrade to "ready".
    if !inp.registry_loaded {
        let mut st = TouchIdStatus::of(
            TouchIdState::ReenrollRequired,
            Some(TouchIdReason::RegistryUnloaded),
        );
        // Uniform can_enroll law: every re-enroll arm uses gateway_ready.
        st.can_enroll = inp.gateway_ready;
        return st;
    }
    if !inp.registry_matches_enrollment {
        let mut st = TouchIdStatus::of(
            TouchIdState::ReenrollRequired,
            Some(TouchIdReason::RegistryMismatch),
        );
        st.can_enroll = inp.gateway_ready;
        return st;
    }

    // 5. Live lease.
    let mut st = TouchIdStatus::of(TouchIdState::Ready, None);
    st.enrolled = true;
    st.allow_real_arm = inp.allow_real_arm;
    st.armed_exp_at_ms = inp.armed_exp_at_ms;
    st.armed_expires_in_ms = inp.armed_expires_in_ms;
    st.stage_expires_in_ms = inp.stage_expires_in_ms;

    if inp.armed {
        // An armed producer whose signed lease has already run out is reported
        // as expired, not as armed: the reserve gate refuses on the same
        // deadline, so presenting "armed" would be a lie in the UI.
        if inp.armed_expires_in_ms == Some(0) {
            st.state = TouchIdState::Ready;
            st.reason = Some(TouchIdReason::LeaseExpired);
            st.can_arm = true;
            st.can_disarm = true;
            return st;
        }
        st.state = TouchIdState::Armed;
        st.can_renew = true;
        st.can_disarm = true;
        return st;
    }

    if inp.ceremony_open {
        st.state = TouchIdState::CeremonyOpen;
        st.can_arm = true;
        // Disarm is always available while a ceremony is open: it is the kill
        // switch and it also clears the open stage.
        st.can_disarm = true;
        return st;
    }

    if !inp.allow_real_arm {
        // Still offer the ceremony (it runs as an audited rehearsal), but say so.
        st.reason = Some(TouchIdReason::RehearsalOnlyBuild);
    }
    st.can_arm = true;
    st
}

// ---------------------------------------------------------------------------
// Enrollment record + registry (app-owned, public material only)
// ---------------------------------------------------------------------------

/// One PUBLIC credential record, byte-compatible with the registry the sidecar
/// loads and with `gateway/bin/arm-enroll` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRecord {
    pub credential_id: String,
    pub credential_type: String,
    /// base64 SEC1 point — a PUBLIC key. No private material exists here.
    pub public_key: String,
    pub label: String,
    pub enrolled_at: String,
}

/// The app-owned enrollment record. Pins the helper's code identity and the
/// registry digest observed at enrollment so a later mismatch is provable
/// rather than assumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentRecord {
    pub credential_id: String,
    pub credential_type: String,
    pub enrolled_at: String,
    /// SHA-256 of the helper binary that produced this enrollment.
    pub helper_sha256: String,
    /// SHA-256 over the canonical registry bytes at enrollment time.
    pub keyset_hash: String,
}

pub fn enrollment_record_path() -> PathBuf {
    gateway_data_dir().join(ENROLLMENT_RECORD_NAME)
}

/// Fail-closed parse of the registry a credential record may join.
///
/// Mirrors the sidecar's rules so the app cannot write a file the sidecar will
/// reject: known types only, base64 SEC1 33/65-byte points, no duplicate ids,
/// non-empty. Values must be ASCII so the canonical form below is unambiguous.
pub fn validate_registry(creds: &[CredentialRecord]) -> Result<(), String> {
    if creds.is_empty() {
        return Err("registry must hold at least one credential".to_string());
    }
    let mut ids = std::collections::BTreeSet::new();
    for c in creds {
        for (name, value) in [
            ("credential_id", &c.credential_id),
            ("credential_type", &c.credential_type),
            ("public_key", &c.public_key),
            ("label", &c.label),
            ("enrolled_at", &c.enrolled_at),
        ] {
            if value.is_empty() {
                return Err(format!("credential field {name} is empty"));
            }
            if !value.is_ascii() || value.chars().any(|ch| ch.is_control()) {
                return Err(format!("credential field {name} is not printable ASCII"));
            }
        }
        if c.credential_type != "se-p256" && c.credential_type != "fido2-es256" {
            return Err("unknown credential_type".to_string());
        }
        if !c.credential_id.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("credential_id is not hex".to_string());
        }
        let decoded_len = base64_decoded_len(&c.public_key)
            .ok_or_else(|| "public_key is not valid base64".to_string())?;
        if decoded_len != 33 && decoded_len != 65 {
            return Err("public_key is not a 33/65-byte SEC1 point".to_string());
        }
        if !ids.insert(c.credential_id.clone()) {
            return Err("duplicate credential_id".to_string());
        }
    }
    Ok(())
}

/// Canonical registry bytes: records sorted by `credential_id`, object keys
/// sorted, no insignificant whitespace. For the ASCII-only field values
/// `validate_registry` enforces, this is byte-identical to the JCS form the
/// sidecar hashes — which is what makes the digest comparison a real proof of
/// "the running sidecar loaded exactly this file" rather than a guess.
pub fn canonical_registry_bytes(creds: &[CredentialRecord]) -> Result<Vec<u8>, String> {
    validate_registry(creds)?;
    let mut sorted = creds.to_vec();
    sorted.sort_by(|a, b| a.credential_id.cmp(&b.credential_id));
    let canonical: Vec<BTreeMap<&str, &str>> = sorted
        .iter()
        .map(|c| {
            BTreeMap::from([
                ("credential_id", c.credential_id.as_str()),
                ("credential_type", c.credential_type.as_str()),
                ("public_key", c.public_key.as_str()),
                ("label", c.label.as_str()),
                ("enrolled_at", c.enrolled_at.as_str()),
            ])
        })
        .collect();
    serde_json::to_vec(&canonical).map_err(|e| format!("canonicalize registry: {e}"))
}

/// The keyset digest the sidecar will report for this registry.
pub fn registry_keyset_hash(creds: &[CredentialRecord]) -> Result<String, String> {
    let bytes = canonical_registry_bytes(creds)?;
    Ok(hex_lower(sha2::Sha256::digest(&bytes).as_slice()))
}

/// Length of the bytes `input` decodes to, or `None` when it is not valid
/// standard base64. Only the LENGTH is needed (SEC1 point shape), so no decoder
/// dependency is pulled in for a shape check.
fn base64_decoded_len(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut pad = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        let is_pad = *b == b'=';
        if is_pad {
            if i < bytes.len() - 2 {
                return None;
            }
            pad += 1;
        } else {
            if pad > 0 {
                return None;
            }
            if !(b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/') {
                return None;
            }
        }
    }
    Some(bytes.len() / 4 * 3 - pad)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Helper resolution and identity
// ---------------------------------------------------------------------------

/// Candidate paths for the bundled signing helper.
///
/// `Contents/Helpers` is the production location: Apple treats it as nested
/// code, so inside-out signing and verification discover the Mach-O normally.
/// The Resources candidates are compatibility-only for earlier local 0.1.2
/// rehearsals; a production verifier requires the Helpers location.
fn helper_candidate_paths(mac_os: &Path) -> Option<[PathBuf; 3]> {
    let contents = mac_os.parent()?;
    let resources = contents.join("Resources");
    Some([
        contents.join("Helpers").join(HELPER_BIN_NAME),
        resources.join(HELPER_BIN_NAME),
        resources.join("resources").join(HELPER_BIN_NAME),
    ])
}

/// Path of the bundled signing helper.
///
/// Release builds resolve only inside the app bundle. `IRIN_TOUCH_ID_HELPER`
/// is a debug-only unit-test escape hatch, never a production input.
pub fn helper_path() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        if let Ok(p) = std::env::var("IRIN_TOUCH_ID_HELPER") {
            let p = PathBuf::from(p.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    helper_candidate_paths(&executable_dir()?)?
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// SHA-256 of the helper binary — the code-identity pinned at enrollment.
pub fn helper_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read helper: {e}"))?;
    Ok(hex_lower(sha2::Sha256::digest(&bytes).as_slice()))
}

fn arm_attest_config_dir() -> PathBuf {
    gateway_data_dir().join(ARM_ATTEST_DIR_NAME)
}

/// The Secure Enclave wrapped-key blob path. The app never reads or copies its
/// contents. Explicit re-enrollment may atomically rename it to a dated archive.
fn enclave_key_path() -> PathBuf {
    arm_attest_config_dir().join(ENCLAVE_KEY_NAME)
}

/// Atomically move an existing custody artifact aside without ever reading,
/// copying, or deleting it. A suffix collision is resolved locally rather than
/// overwriting an earlier archive.
fn archive_existing(path: &Path, timestamp_ms: u128) -> Result<Option<PathBuf>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("custody artifact has an invalid file name")?;
    for suffix in 0..100u8 {
        let archive_name = if suffix == 0 {
            format!("{name}.archive-{timestamp_ms}")
        } else {
            format!("{name}.archive-{timestamp_ms}-{suffix}")
        };
        let destination = path.with_file_name(archive_name);
        if destination.exists() {
            continue;
        }
        std::fs::rename(path, &destination)
            .map_err(|e| format!("archive custody artifact: {e}"))?;
        return Ok(Some(destination));
    }
    Err("too many custody archives share the same timestamp".to_string())
}

/// Archive the enrollment record first, then the opaque Secure Enclave wrapper.
/// If the second rename is interrupted, the record is already absent, so the
/// state machine cannot report Ready or attempt to arm with ambiguous custody.
fn archive_enrollment_for_replacement(timestamp_ms: u128) -> Result<(), String> {
    archive_existing(&enrollment_record_path(), timestamp_ms)?;
    archive_existing(&enclave_key_path(), timestamp_ms)?;
    Ok(())
}

/// Run the helper with a minimal environment and a hard timeout.
///
/// `env_clear` keeps provider keys, admin tokens, and Gateway credentials out
/// of the helper process entirely. The installed app pins helper state to its
/// Application Support directory; the legacy CLI default is never inherited.
fn run_helper(helper: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(helper);
    cmd.args(args);
    cmd.env_clear();
    cmd.env("IRIN_ARM_ATTEST_DIR", arm_attest_config_dir());
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    let out = run_command_timeout(cmd, HELPER_TIMEOUT)?;
    if !out.status.success() {
        // stderr may quote a path; it never contains key material, but it is
        // still not forwarded verbatim to the renderer by the callers.
        return Err(format!("helper exited {}", out.status.code().unwrap_or(-1)));
    }
    String::from_utf8(out.stdout).map_err(|_| "helper output is not UTF-8".to_string())
}

/// Parse the helper's enroll output. Anything but exactly one well-formed JSON
/// credential record is a hard failure — a compromised or truncated helper must
/// not be able to smuggle bytes into the registry.
pub fn parse_enroll_output(raw: &str) -> Result<CredentialRecord, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("helper produced no enrollment record".to_string());
    }
    let rec: CredentialRecord =
        serde_json::from_str(trimmed).map_err(|_| "malformed enrollment record".to_string())?;
    validate_registry(std::slice::from_ref(&rec))?;
    if rec.credential_type != "se-p256" {
        return Err("enrollment record is not a Secure Enclave credential".to_string());
    }
    Ok(rec)
}

/// The confirm-body fragment the helper emits after a Touch ID signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignFragment {
    pub credential_id: String,
    pub credential_type: String,
    pub signature: String,
    #[serde(default)]
    pub authenticator_data: Option<String>,
}

/// Parse the helper's sign output and bind it to the credential we enrolled.
///
/// Fail-closed on: non-JSON, missing fields, a non-base64 signature, and — the
/// substantive check — a `credential_id` that is not the enrolled one. A helper
/// that returns a signature from a different key can never reach `confirm`.
pub fn parse_sign_output(raw: &str, expected_credential_id: &str) -> Result<SignFragment, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("helper produced no signature".to_string());
    }
    let frag: SignFragment =
        serde_json::from_str(trimmed).map_err(|_| "malformed helper signature".to_string())?;
    if frag.credential_type != "se-p256" {
        return Err("helper signature is not a Secure Enclave signature".to_string());
    }
    if frag.credential_id != expected_credential_id {
        return Err("helper signed with an unenrolled credential".to_string());
    }
    if base64_decoded_len(&frag.signature).is_none() {
        return Err("helper signature is not valid base64".to_string());
    }
    Ok(frag)
}

// ---------------------------------------------------------------------------
// Sidecar ceremony transport
// ---------------------------------------------------------------------------

/// The sidecar's renderer-safe status projection (mirrors `ArmStatusView`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArmStatusView {
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub armed_exp_at_ms: Option<i64>,
    #[serde(default)]
    pub armed_expires_in_ms: Option<u64>,
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub stage_expires_in_ms: Option<u64>,
    #[serde(default)]
    pub registry_loaded: bool,
    #[serde(default)]
    pub keyset_hash: Option<String>,
    #[serde(default)]
    pub arm_capable: bool,
    #[serde(default)]
    pub allow_real_arm: bool,
}

fn arm_url(suffix: &str) -> String {
    format!("{DESKTOP_GATEWAY_URL}/watch/admin/producer/{suffix}")
}

fn bearer(token: &str) -> String {
    format!("{ARM_PRINCIPAL_NAME}:{token}")
}

fn http_json(
    method: &str,
    url: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> Result<(u16, String), String> {
    let req = match method {
        "GET" => ureq::get(url),
        "POST" => ureq::post(url),
        _ => return Err("unsupported method".to_string()),
    }
    .timeout(HTTP_TIMEOUT)
    .set("Authorization", &format!("Bearer {}", bearer(token)));
    let result = match body {
        Some(v) => req
            .set("Content-Type", "application/json")
            .send_string(&v.to_string()),
        None => req.call(),
    };
    match result {
        Ok(resp) => {
            let code = resp.status();
            Ok((code, resp.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(code, resp)) => Ok((code, resp.into_string().unwrap_or_default())),
        // Transport errors never quote the Authorization header, but they can
        // quote the URL; the callers map this to a fixed tag rather than text.
        Err(_) => Err("watch surface unreachable".to_string()),
    }
}

/// Mint the arm-principal bearer token on first use. Idempotent: an existing
/// valid token is reused so a re-enrollment does not orphan the running pack's
/// registry string.
pub fn ensure_arm_principal(store: &dyn SecretStore) -> Result<String, String> {
    if let Some(existing) = load_arm_principal_token(store)? {
        return Ok(existing);
    }
    let mut raw = [0u8; 16];
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("urandom: {e}"))?;
        f.read_exact(&mut raw)
            .map_err(|e| format!("urandom: {e}"))?;
    }
    let token = format!("tok_{}", hex_lower(&raw));
    debug_assert!(is_valid_arm_principal_token(&token));
    store_arm_principal_token(store, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Status gathering
// ---------------------------------------------------------------------------

fn read_enrollment_record() -> Option<EnrollmentRecord> {
    let raw = std::fs::read_to_string(enrollment_record_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_registry() -> Vec<CredentialRecord> {
    std::fs::read_to_string(arm_keys_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Gather every input the state machine needs. All IO lives here; the decision
/// itself is [`derive_status`]. Authority callers load the bearer live; the
/// background loop uses a lifecycle-scoped presence observation.
pub fn gather_inputs(store: &dyn SecretStore, gateway_ready: bool) -> TouchIdInputs {
    let held = load_arm_principal_token(store).ok().flatten();
    gather_inputs_with_mode(
        store,
        gateway_ready,
        ArmPrincipalProbeMode::AuthorityLive(held.as_deref()),
    )
}

pub fn gather_inputs_background(store: &dyn SecretStore, gateway_ready: bool) -> TouchIdInputs {
    gather_inputs_with_mode(
        store,
        gateway_ready,
        ArmPrincipalProbeMode::BackgroundCached,
    )
}

fn gather_inputs_with_mode(
    store: &dyn SecretStore,
    gateway_ready: bool,
    mode: ArmPrincipalProbeMode<'_>,
) -> TouchIdInputs {
    let mut inp = TouchIdInputs {
        gateway_ready,
        ..Default::default()
    };

    let helper = helper_path();
    inp.helper_present = helper.is_some();
    inp.enclave_key_present = enclave_key_path().is_file();

    let record = read_enrollment_record();
    inp.enrollment_record_present = record.is_some();
    if let (Some(helper), Some(rec)) = (helper.as_ref(), record.as_ref()) {
        inp.helper_identity_matches = helper_sha256(helper)
            .map(|got| got == rec.helper_sha256)
            .unwrap_or(false);
    }
    if let Some(rec) = record.as_ref() {
        inp.local_registry_matches_enrollment = registry_keyset_hash(&read_registry())
            .map(|digest| digest == rec.keyset_hash)
            .unwrap_or(false);
    }

    let (present, token) = resolve_arm_principal(store, mode);
    inp.arm_principal_present = present;
    inp.watch_probe_deferred = present && token.is_none() && gateway_ready;

    // The Watch surface is only probed once the local prerequisites hold: a
    // status poll must not fire an HTTP request on every render while Gateway
    // is off.
    let (Some(token), true) = (token, gateway_ready) else {
        return inp;
    };
    let Ok((code, body)) = http_json("GET", &arm_url("arm/status"), &token, None) else {
        return inp;
    };
    if code != 200 {
        // 401 here means the pack is running without our principal registry
        // (e.g. started before enrollment) — reachable, but not arm-capable.
        inp.watch_reachable = true;
        return inp;
    }
    let Ok(view) = serde_json::from_str::<ArmStatusView>(&body) else {
        return inp;
    };
    inp.watch_reachable = true;
    inp.registry_loaded = view.registry_loaded;
    inp.arm_capable = view.arm_capable;
    inp.allow_real_arm =
        effective_real_arm_permission(view.allow_real_arm, crate::bundled_build_identity().1);
    inp.armed = view.armed;
    inp.armed_exp_at_ms = view.armed_exp_at_ms;
    inp.armed_expires_in_ms = view.armed_expires_in_ms;
    inp.ceremony_open = view.staged;
    inp.stage_expires_in_ms = view.stage_expires_in_ms;

    // Continuity proof: the digest the sidecar reports must equal the digest of
    // the registry file this app owns, AND the enrollment record must pin that
    // same digest. Any drift is a re-enrollment, never a silent pass.
    if let (Some(reported), Some(rec)) = (view.keyset_hash.as_deref(), record.as_ref()) {
        let local = registry_keyset_hash(&read_registry()).ok();
        inp.registry_matches_enrollment =
            local.as_deref() == Some(reported) && reported == rec.keyset_hash;
    }
    inp
}

/// Full status for the renderer.
pub fn touch_id_status(store: &dyn SecretStore, gateway_ready: bool) -> TouchIdStatus {
    touch_id_status_from_inputs(gather_inputs(store, gateway_ready))
}

pub fn touch_id_status_background(
    store: &dyn SecretStore,
    gateway_ready: bool,
    previous: Option<&TouchIdStatus>,
) -> TouchIdStatus {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    project_background_status(
        gather_inputs_background(store, gateway_ready),
        previous,
        now_ms,
    )
}

fn project_background_status(
    inputs: TouchIdInputs,
    previous: Option<&TouchIdStatus>,
    now_ms: i64,
) -> TouchIdStatus {
    if !inputs.watch_probe_deferred {
        return touch_id_status_from_inputs(inputs);
    }

    let mut status = touch_id_status_from_inputs(inputs.clone());
    let Some(previous) = previous else {
        // No authenticated Watch projection exists yet. Stay fail-closed, but
        // do not claim the intentionally skipped request was unreachable.
        if status.reason == Some(TouchIdReason::WatchSurfaceUnreachable) {
            status.reason = None;
        }
        return status;
    };

    // Local custody and Gateway inputs were gathered fresh and outrank the
    // deferred Watch projection. Do not let a prior Ready/Armed sample hide a
    // removed registry, changed helper, missing enclave key, or hard-down pack.
    let local_prerequisites_hold = inputs.helper_present
        && inputs.enrollment_record_present
        && inputs.helper_identity_matches
        && inputs.enclave_key_present
        && inputs.local_registry_matches_enrollment
        && inputs.gateway_ready
        && inputs.arm_principal_present;
    if !local_prerequisites_hold {
        return status;
    }

    // Preserve only fields derived from the skipped authenticated Watch
    // response. Local `can_enroll` and rehearsal projection remain fresh.
    status.state = previous.state;
    status.reason = previous.reason;
    status.armed_exp_at_ms = previous.armed_exp_at_ms;
    status.armed_expires_in_ms = previous.armed_expires_in_ms;
    status.stage_expires_in_ms = previous.stage_expires_in_ms;
    status.enrolled = previous.enrolled;
    status.allow_real_arm = previous.allow_real_arm;
    status.can_arm = previous.can_arm;
    status.can_renew = previous.can_renew;
    status.can_disarm = previous.can_disarm;
    if let Some(deadline) = status.armed_exp_at_ms {
        let remaining = deadline.saturating_sub(now_ms).max(0) as u64;
        status.armed_expires_in_ms = Some(remaining);
        if status.state == TouchIdState::Armed && remaining == 0 {
            status.state = TouchIdState::Ready;
            status.reason = Some(TouchIdReason::LeaseExpired);
            status.can_arm = true;
            status.can_renew = false;
            status.can_disarm = true;
        }
    }
    status
}

fn touch_id_status_from_inputs(inputs: TouchIdInputs) -> TouchIdStatus {
    let mut st = derive_status(&inputs);
    // Real armed leases always clear the rehearsal sticky; otherwise surface it
    // only while the control is in a ready/blocked post-ceremony state.
    if st.state == TouchIdState::Armed {
        clear_rehearsal_passed();
        st.rehearsal_passed = false;
    } else if matches!(
        st.state,
        TouchIdState::Ready | TouchIdState::CeremonyOpen | TouchIdState::Blocked
    ) {
        st.rehearsal_passed = rehearsal_passed_sticky();
    }
    st
}

// ---------------------------------------------------------------------------
// Ceremonies
// ---------------------------------------------------------------------------

/// Enrollment: Touch ID proves the biometric gate, the helper returns a PUBLIC
/// credential record, and the app-owned registry is rewritten atomically.
///
/// Replaces rather than appends: a single-operator desktop registry with a
/// stale credential from a previous helper identity would keep a key we can no
/// longer prove continuity for. Existing app-owned custody artifacts are moved
/// to dated archives before the helper creates a replacement; they are never
/// read, copied, deleted, or silently adopted.
pub fn enroll(store: &dyn SecretStore, gateway_ready: bool) -> Result<TouchIdStatus, String> {
    if !gateway_ready {
        return Err("Gateway must be enabled and authenticated before Touch ID setup".to_string());
    }
    clear_rehearsal_passed();
    let helper = helper_path().ok_or("Touch ID helper is not bundled in this app")?;
    let digest = helper_sha256(&helper)?;

    // Re-enrollment changes the credential that can ratify a stage. Prove the
    // existing producer is disarmed before changing custody; failure blocks the
    // replacement instead of assuming that default-off is enough.
    if read_enrollment_record().is_some() {
        disarm(store)?;
    }
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_millis();
    archive_enrollment_for_replacement(timestamp_ms)?;

    let raw = run_helper(&helper, &["enroll", "--label", "irin-desktop-touch-id"])?;
    let rec = parse_enroll_output(&raw)?;

    let creds = vec![rec.clone()];
    let keyset_hash = registry_keyset_hash(&creds)?;
    let bytes = canonical_registry_bytes(&creds)?;
    write_atomic_0600(&arm_keys_path(), &bytes)?;

    let enrollment = EnrollmentRecord {
        credential_id: rec.credential_id.clone(),
        credential_type: rec.credential_type.clone(),
        enrolled_at: rec.enrolled_at.clone(),
        helper_sha256: digest,
        keyset_hash,
    };
    let encoded = serde_json::to_vec(&enrollment).map_err(|e| format!("encode enrollment: {e}"))?;
    write_atomic_0600(&enrollment_record_path(), &encoded)?;

    // Custody domain 1 exists only from here on.
    ensure_arm_principal(store)?;

    // The caller must explicitly refresh the Gateway Pack after this returns:
    // the sidecar loads the registry and arm-principal allowlist at boot.
    Ok(touch_id_status(store, gateway_ready))
}

/// Parse stage_id + challenge from a stage or pending JSON body.
fn stage_id_and_challenge(body: &str, what: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| format!("malformed {what} response"))?;
    let sid = v
        .get("stage_id")
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("{what} response is missing stage_id"))?
        .to_string();
    let ch = v
        .get("challenge")
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("{what} response is missing challenge"))?
        .to_string();
    Ok((sid, ch))
}

fn post_fresh_stage(token: &str, host_rehearsal: bool) -> Result<(String, String), String> {
    let (code, body) = http_json(
        "POST",
        &arm_url("arm/stage"),
        token,
        stage_request_body(host_rehearsal),
    )?;
    if code != 200 {
        return Err(format!("stage refused ({code})"));
    }
    stage_id_and_challenge(&body, "stage")
}

/// Arm (or renew) through the existing stage → sign → confirm ceremony.
///
/// Resume-first for compatible stages: an open unexpired stage is RESUMED with
/// its stored challenge bytes rather than re-staged, so a retry never fires a
/// second Touch ID prompt against a fresh nonce. Exceptions:
/// - dirty host + real pending stage → refuse (operator Disarms first)
/// - clean host + rehearsal pending stage → POST a fresh real stage (sidecar
///   atomically replaces the pending row)
///
/// The ceremony is also bound to the current pack lifecycle generation: if the
/// pack identity changes between stage and confirm, the arm fails closed.
pub fn arm(store: &dyn SecretStore) -> Result<TouchIdStatus, String> {
    let ceremony = TOUCH_ID_CEREMONY_FENCE.begin_ceremony()?;
    let helper = helper_path().ok_or("Touch ID helper is not bundled in this app")?;
    let record = read_enrollment_record().ok_or("Touch ID is not enrolled")?;
    if helper_sha256(&helper)? != record.helper_sha256 {
        return Err("Touch ID helper identity changed; re-enroll before arming".to_string());
    }
    let token =
        load_arm_principal_token(store)?.ok_or("Touch ID arm principal is missing; re-enroll")?;

    // Bind ceremony to the pack generation observed at stage time.
    let pack_gen = crate::gateway_pack::pack_lifecycle_generation();

    // 1. Resume or stage. A dirty host must never resume a real-arm stage
    // created by a clean host; the operator clears it with the always-available
    // Disarm kill switch. A clean host must never resume a rehearsal stage —
    // it posts a fresh real stage instead. Fresh stages from a dirty host
    // explicitly request rehearsal even when paired with a production sidecar.
    let host_rehearsal = host_requires_rehearsal();
    let (stage_id, challenge) = TOUCH_ID_CEREMONY_FENCE.run_if_current(ceremony, || {
        match http_json("GET", &arm_url("arm/pending"), &token, None)? {
            (200, body) => {
                let v: serde_json::Value =
                    serde_json::from_str(&body).map_err(|_| "malformed pending response")?;
                let stage_rehearsal = v
                    .get("rehearsal")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                match classify_pending_stage(host_rehearsal, stage_rehearsal) {
                    PendingStageDisposition::RefuseRealStageOnDirtyHost => Err(
                        "A real-arm stage is already open. Use Disarm to clear it before running this rehearsal build."
                            .to_string(),
                    ),
                    PendingStageDisposition::StageFresh => {
                        // Clean host refuses to resume a rehearsal stage; the
                        // sidecar stage op atomically replaces the pending row.
                        post_fresh_stage(&token, host_rehearsal)
                    }
                    PendingStageDisposition::Resume => stage_id_and_challenge(&body, "pending"),
                }
            }
            (404, _) => post_fresh_stage(&token, host_rehearsal),
            (code, _) => Err(format!("stage lookup refused ({code})")),
        }
    })?;

    // Pack generation must still match after staging.
    if crate::gateway_pack::pack_lifecycle_generation() != pack_gen {
        return Err(
            "Gateway Pack changed during the Touch ID ceremony; arm aborted. Enable Gateway, then arm again."
                .to_string(),
        );
    }

    // Avoid opening a biometric prompt when Disarm won immediately after the
    // stage boundary. A later race is still caught at the confirm boundary.
    TOUCH_ID_CEREMONY_FENCE.ensure_current(ceremony)?;

    // 2. Touch ID. The helper enforces its own rate limit; the challenge bytes
    //    are passed through verbatim and never re-canonicalized here.
    let raw = run_helper(
        &helper,
        &["sign", "--challenge", &challenge, "--stage-id", &stage_id],
    )?;
    let frag = parse_sign_output(&raw, &record.credential_id)?;

    // Pack generation must still match after the biometric prompt.
    if crate::gateway_pack::pack_lifecycle_generation() != pack_gen {
        return Err(
            "Gateway Pack changed during the Touch ID ceremony; arm aborted. Enable Gateway, then arm again."
                .to_string(),
        );
    }

    // 3. Confirm, bound to THIS stage. A stage_id the sidecar no longer holds
    //    is a 409/410 there — a replayed fragment can never ratify a new stage.
    let body = serde_json::json!({
        "stage_id": stage_id,
        "credential_id": frag.credential_id,
        "credential_type": frag.credential_type,
        "signature": frag.signature,
        "authenticator_data": frag.authenticator_data,
    });
    let (code, resp) = TOUCH_ID_CEREMONY_FENCE.run_if_current(ceremony, || {
        http_json("POST", &arm_url("arm/confirm"), &token, Some(body))
    })?;
    if code != 200 {
        return Err(format!("arm not confirmed ({code})"));
    }
    // Final pack-generation check before accepting a green outcome.
    if crate::gateway_pack::pack_lifecycle_generation() != pack_gen {
        return Err(
            "Gateway Pack changed during the Touch ID ceremony; arm aborted. Enable Gateway, then arm again."
                .to_string(),
        );
    }
    let status = serde_json::from_str::<serde_json::Value>(&resp)
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_default();
    if status == "rehearsal-ok" {
        note_rehearsal_ok();
    } else if status == "armed" {
        clear_rehearsal_passed();
    } else {
        return Err("arm did not complete".to_string());
    }
    Ok(touch_id_status(store, true))
}

/// Renew is the same ceremony as [`arm`]: a lease is never extended without a
/// fresh Touch ID tap, a fresh stage nonce, and a fresh audited confirm. It is a
/// named entry point purely so the product control can label the action.
pub fn renew(store: &dyn SecretStore) -> Result<TouchIdStatus, String> {
    arm(store)
}

/// Immediate disarm. Single-principal by design — a kill switch never waits.
pub fn disarm(store: &dyn SecretStore) -> Result<TouchIdStatus, String> {
    let (code, _) = TOUCH_ID_CEREMONY_FENCE.cancel_and_run(|| {
        let token = load_arm_principal_token(store)?
            .ok_or("Touch ID arm principal is missing; re-enroll")?;
        http_json("POST", &arm_url("disarm"), &token, None)
    })?;
    if code != 200 {
        return Err(format!("disarm refused ({code})"));
    }
    clear_rehearsal_passed();
    Ok(touch_id_status(store, true))
}

/// 0600 atomic write in the app-owned gateway dir.
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("touch-id"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename into place: {e}")
    })
}

/// The in-container registry path, re-exported for the pack contract test.
#[cfg(test)]
pub const REGISTRY_CONTAINER_PATH: &str = ARM_KEYS_CONTAINER_PATH;

#[cfg(test)]
mod tests {
    use super::*;

    /// A 33-byte compressed SEC1 point, base64.
    fn pubkey_b64() -> String {
        // 33 bytes -> 44 base64 chars with one '=' pad.
        let bytes = [2u8; 33];
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
            for (i, v) in idx.iter().enumerate() {
                if i > chunk.len() {
                    out.push('=');
                } else {
                    out.push(table[*v as usize] as char);
                }
            }
        }
        out
    }

    fn cred(id: &str) -> CredentialRecord {
        CredentialRecord {
            credential_id: id.to_string(),
            credential_type: "se-p256".to_string(),
            public_key: pubkey_b64(),
            label: "irin-desktop-touch-id".to_string(),
            enrolled_at: "2026-07-24T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn host_and_sidecar_must_both_allow_real_arming() {
        assert!(!effective_real_arm_permission(false, false));
        assert!(!effective_real_arm_permission(false, true));
        assert!(!effective_real_arm_permission(true, true));
        assert!(effective_real_arm_permission(true, false));
    }

    #[test]
    fn stage_request_body_uses_sidecar_rehearse_key() {
        let body = stage_request_body(true).expect("dirty host stages rehearsal");
        assert_eq!(body, serde_json::json!({ "rehearse": true }));
        assert!(
            body.get("rehearsal").is_none(),
            "must not send the wrong key the sidecar ignores"
        );
        assert!(stage_request_body(false).is_none());
    }

    #[test]
    fn pending_stage_disposition_matches_resume_policy() {
        // Dirty host + real stage → refuse.
        assert_eq!(
            classify_pending_stage(true, false),
            PendingStageDisposition::RefuseRealStageOnDirtyHost
        );
        // Dirty host + rehearsal stage → resume.
        assert_eq!(
            classify_pending_stage(true, true),
            PendingStageDisposition::Resume
        );
        // Clean host + real stage → resume.
        assert_eq!(
            classify_pending_stage(false, false),
            PendingStageDisposition::Resume
        );
        // Clean host + rehearsal stage → POST fresh real stage (no resume).
        assert_eq!(
            classify_pending_stage(false, true),
            PendingStageDisposition::StageFresh
        );
    }

    #[test]
    fn stale_ceremony_is_refused_before_staging_after_disarm() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let fence = CeremonyFence::new();
        let stale = fence.begin_ceremony().unwrap();
        fence.cancel_and_run(|| Ok(())).unwrap();

        let stage_called = Arc::new(AtomicBool::new(false));
        let stage_called_in_op = Arc::clone(&stage_called);
        let error = fence
            .run_if_current(stale, move || {
                stage_called_in_op.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains("cancelled by Disarm"), "{error}");
        assert!(
            !stage_called.load(Ordering::SeqCst),
            "a stale ceremony must be refused before stage/resume transport"
        );
    }

    #[test]
    fn blocked_ceremony_cannot_confirm_after_disarm_then_resume() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};

        let fence = Arc::new(CeremonyFence::new());
        let confirm_called = Arc::new(AtomicBool::new(false));
        let (blocked_tx, blocked_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);

        let worker_fence = Arc::clone(&fence);
        let worker_confirm_called = Arc::clone(&confirm_called);
        let worker = std::thread::spawn(move || {
            let ceremony = worker_fence.begin_ceremony().unwrap();
            // This models the arm/renew worker blocked in the Touch ID helper.
            blocked_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            worker_fence.run_if_current(ceremony, move || {
                worker_confirm_called.store(true, Ordering::SeqCst);
                Ok(())
            })
        });

        blocked_rx.recv().unwrap();
        fence
            .cancel_and_run(|| {
                assert!(
                    fence.begin_ceremony().is_err(),
                    "new ceremonies stay blocked until Disarm returns"
                );
                Ok(())
            })
            .unwrap();
        resume_tx.send(()).unwrap();

        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("cancelled by Disarm"), "{error}");
        assert!(
            !confirm_called.load(Ordering::SeqCst),
            "a helper result from before Disarm must never reach confirm"
        );

        let after_disarm = fence.begin_ceremony().unwrap();
        fence
            .run_if_current(after_disarm, || Ok(()))
            .expect("a new explicit ceremony after Disarm may proceed");
    }

    fn enrolled_inputs() -> TouchIdInputs {
        TouchIdInputs {
            helper_present: true,
            helper_identity_matches: true,
            enclave_key_present: true,
            enrollment_record_present: true,
            local_registry_matches_enrollment: true,
            gateway_ready: true,
            arm_principal_present: true,
            watch_reachable: true,
            registry_loaded: true,
            registry_matches_enrollment: true,
            arm_capable: true,
            allow_real_arm: true,
            ..Default::default()
        }
    }

    #[test]
    fn helper_candidates_prefer_the_standard_nested_code_location() {
        let mac_os = Path::new("/Applications/IRIN.app/Contents/MacOS");
        let candidates = helper_candidate_paths(mac_os).unwrap();
        assert_eq!(
            candidates[0],
            PathBuf::from("/Applications/IRIN.app/Contents/Helpers/arm-attest")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from("/Applications/IRIN.app/Contents/Resources/arm-attest")
        );
    }

    #[test]
    fn sticky_readiness_holds_soft_failures_but_demotes_hard_down() {
        let mut sticky = GatewayReadySticky::default();
        let hold = GatewayReadySticky::DEFAULT_HOLD_MS;
        // First true sample arms the hold.
        assert!(sticky.project(true, false, 1_000, hold));
        // Soft failure (Degraded / auth flake) within the hold stays true.
        assert!(sticky.project(false, false, 1_000 + 8_000, hold));
        assert!(sticky.project(false, false, 1_000 + 19_999, hold));
        // After the hold expires, soft failure demotes.
        assert!(!sticky.project(false, false, 1_000 + hold + 1, hold));
        // A later true re-arms.
        assert!(sticky.project(true, false, 50_000, hold));
        // Hard-down (Disable / Docker missing) demotes immediately.
        assert!(!sticky.project(false, true, 50_100, hold));
        // And does not resurrect from the cleared sticky.
        assert!(!sticky.project(false, false, 50_200, hold));
    }

    #[test]
    fn can_enroll_is_uniformly_gateway_ready_on_all_reenroll_arms() {
        // Every re-enroll / setup arm that offers can_enroll must follow one law:
        // can_enroll == gateway_ready. Cover all reasons that set the flag.
        type ReenrollCase = (&'static str, fn(&mut TouchIdInputs));
        let reenroll_cases: &[ReenrollCase] = &[
            ("enrollment_missing_setup", |i| {
                i.enrollment_record_present = false;
                i.enclave_key_present = false;
            }),
            ("enrollment_missing_reenroll", |i| {
                i.enrollment_record_present = false;
                i.enclave_key_present = true;
            }),
            ("helper_identity_changed", |i| {
                i.helper_identity_matches = false;
            }),
            ("enclave_key_missing", |i| {
                i.enclave_key_present = false;
            }),
            ("registry_unloaded", |i| {
                i.registry_loaded = false;
            }),
            ("registry_mismatch", |i| {
                i.registry_matches_enrollment = false;
            }),
        ];

        for (label, mutate) in reenroll_cases {
            let mut base = TouchIdInputs {
                helper_present: true,
                enrollment_record_present: true,
                helper_identity_matches: true,
                enclave_key_present: true,
                gateway_ready: false,
                watch_reachable: true,
                arm_principal_present: true,
                arm_capable: true,
                registry_loaded: true,
                registry_matches_enrollment: true,
                ..Default::default()
            };
            mutate(&mut base);
            assert!(
                !derive_status(&base).can_enroll,
                "{label} without gateway_ready"
            );
            base.gateway_ready = true;
            assert!(
                derive_status(&base).can_enroll,
                "{label} with gateway_ready"
            );
        }
    }

    #[test]
    fn sticky_uses_monotonic_deltas_not_wall_clock_regression() {
        let mut sticky = GatewayReadySticky::default();
        let hold = GatewayReadySticky::DEFAULT_HOLD_MS;
        assert!(sticky.project(true, false, 10_000, hold));
        // Forward monotonic progress within hold.
        assert!(sticky.project(false, false, 10_000 + 5_000, hold));
        // saturating_sub: if a caller ever fed a smaller now_ms (wall-clock
        // step), elapsed collapses to 0 and would hold — production uses
        // monotonic Instant origin so now_ms only increases.
        assert!(sticky.project(false, false, 10_000 + hold, hold));
        assert!(!sticky.project(false, false, 10_000 + hold + 1, hold));
    }

    #[test]
    fn no_helper_offers_nothing() {
        let st = derive_status(&TouchIdInputs::default());
        assert_eq!(st.state, TouchIdState::Unavailable);
        assert_eq!(st.reason, Some(TouchIdReason::HelperMissing));
        assert!(!st.can_enroll && !st.can_arm && !st.can_renew && !st.can_disarm);
    }

    #[test]
    fn fresh_host_asks_for_setup_but_waits_for_gateway() {
        let mut inp = TouchIdInputs {
            helper_present: true,
            ..Default::default()
        };
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::SetupRequired);
        assert!(!st.can_enroll);
        assert!(!st.can_arm, "setup must never expose an arm action");

        inp.gateway_ready = true;
        assert!(derive_status(&inp).can_enroll);
    }

    /// The old ad-hoc helper case: an enclave blob with no app-owned record is
    /// never adopted — it demands explicit re-enrollment.
    #[test]
    fn orphan_enclave_blob_demands_reenrollment() {
        let inp = TouchIdInputs {
            helper_present: true,
            enclave_key_present: true,
            gateway_ready: true,
            ..Default::default()
        };
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::EnrollmentMissing));
        assert!(st.can_enroll);
        assert!(!st.can_arm);
    }

    #[test]
    fn helper_identity_change_demands_reenrollment() {
        let mut inp = enrolled_inputs();
        inp.helper_identity_matches = false;
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::HelperIdentityChanged));
        assert!(!st.can_arm);
    }

    #[test]
    fn missing_enclave_blob_demands_reenrollment() {
        let mut inp = enrolled_inputs();
        inp.enclave_key_present = false;
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::EnclaveKeyMissing));
    }

    #[test]
    fn prerequisite_failures_have_distinct_reasons() {
        type InputMutator = fn(&mut TouchIdInputs);
        let cases: [(InputMutator, TouchIdReason); 4] = [
            (
                |i: &mut TouchIdInputs| i.gateway_ready = false,
                TouchIdReason::GatewayNotReady,
            ),
            (
                |i: &mut TouchIdInputs| i.watch_reachable = false,
                TouchIdReason::WatchSurfaceUnreachable,
            ),
            (
                |i: &mut TouchIdInputs| i.arm_principal_present = false,
                TouchIdReason::ArmPrincipalMissing,
            ),
            (
                |i: &mut TouchIdInputs| i.arm_capable = false,
                TouchIdReason::ArmPrincipalMissing,
            ),
        ];
        for (mutate, expected) in cases {
            let mut inp = enrolled_inputs();
            mutate(&mut inp);
            let st = derive_status(&inp);
            assert_eq!(st.state, TouchIdState::Blocked, "{expected:?}");
            assert_eq!(st.reason, Some(expected));
            assert!(!st.can_arm, "a blocked prerequisite must not expose arming");
        }
    }

    /// A pack restart that did not load our registry fails closed.
    #[test]
    fn registry_drift_demands_reenrollment() {
        let mut unloaded = enrolled_inputs();
        unloaded.registry_loaded = false;
        let st = derive_status(&unloaded);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::RegistryUnloaded));
        assert!(!st.can_arm);

        let mut mismatched = enrolled_inputs();
        mismatched.registry_matches_enrollment = false;
        let st = derive_status(&mismatched);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::RegistryMismatch));
        assert!(!st.can_arm);
    }

    #[test]
    fn enrolled_and_disarmed_is_ready_and_armable() {
        let st = derive_status(&enrolled_inputs());
        assert_eq!(st.state, TouchIdState::Ready);
        assert_eq!(st.reason, None);
        assert!(st.enrolled && st.can_arm);
        assert!(!st.can_renew && !st.can_disarm);
    }

    #[test]
    fn repeated_background_cache_ticks_preserve_last_watch_projection() {
        let previous = derive_status(&enrolled_inputs());
        let mut deferred = enrolled_inputs();
        deferred.watch_reachable = false;
        deferred.watch_probe_deferred = true;

        let first = project_background_status(deferred.clone(), Some(&previous), 1_000);
        let second = project_background_status(deferred, Some(&first), 2_000);

        assert_eq!(first.state, TouchIdState::Ready);
        assert_eq!(first.reason, None);
        assert_eq!(second, first);
    }

    #[test]
    fn deferred_watch_probe_does_not_hide_fresh_enrollment_record_removal() {
        let previous = derive_status(&enrolled_inputs());
        let mut deferred = enrolled_inputs();
        deferred.enrollment_record_present = false;
        deferred.watch_reachable = false;
        deferred.watch_probe_deferred = true;

        let current = project_background_status(deferred, Some(&previous), 1_000);
        assert_eq!(current.state, TouchIdState::ReenrollRequired);
        assert_eq!(current.reason, Some(TouchIdReason::EnrollmentMissing));
        assert!(!current.enrolled);
        assert!(!current.can_arm);
    }

    #[test]
    fn deferred_watch_probe_does_not_hide_fresh_local_registry_drift() {
        let previous = derive_status(&enrolled_inputs());
        let mut deferred = enrolled_inputs();
        deferred.local_registry_matches_enrollment = false;
        deferred.watch_reachable = false;
        deferred.watch_probe_deferred = true;

        let current = project_background_status(deferred, Some(&previous), 1_000);
        assert_eq!(current.state, TouchIdState::ReenrollRequired);
        assert_eq!(current.reason, Some(TouchIdReason::RegistryMismatch));
        assert!(!current.enrolled);
        assert!(!current.can_arm);
    }

    #[test]
    fn deferred_watch_probe_does_not_hide_fresh_enclave_key_removal() {
        let previous = derive_status(&enrolled_inputs());
        let mut deferred = enrolled_inputs();
        deferred.enclave_key_present = false;
        deferred.watch_reachable = false;
        deferred.watch_probe_deferred = true;

        let current = project_background_status(deferred, Some(&previous), 1_000);
        assert_eq!(current.state, TouchIdState::ReenrollRequired);
        assert_eq!(current.reason, Some(TouchIdReason::EnclaveKeyMissing));
        assert!(!current.enrolled);
        assert!(!current.can_arm);
    }

    #[test]
    fn deferred_background_tick_expires_cached_armed_lease_without_watch_probe() {
        let mut armed = enrolled_inputs();
        armed.armed = true;
        armed.armed_exp_at_ms = Some(10_000);
        armed.armed_expires_in_ms = Some(5_000);
        let previous = derive_status(&armed);

        let mut deferred = enrolled_inputs();
        deferred.watch_reachable = false;
        deferred.watch_probe_deferred = true;
        let expired = project_background_status(deferred, Some(&previous), 10_000);

        assert_eq!(expired.state, TouchIdState::Ready);
        assert_eq!(expired.reason, Some(TouchIdReason::LeaseExpired));
        assert_eq!(expired.armed_expires_in_ms, Some(0));
        assert!(expired.can_arm && expired.can_disarm);
    }

    #[test]
    fn armed_exposes_renew_and_disarm_with_a_deadline() {
        let mut inp = enrolled_inputs();
        inp.armed = true;
        inp.armed_exp_at_ms = Some(1_800_000_000_000);
        inp.armed_expires_in_ms = Some(600_000);
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::Armed);
        assert_eq!(st.armed_exp_at_ms, Some(1_800_000_000_000));
        assert!(st.can_renew && st.can_disarm);
        assert!(!st.can_arm, "armed offers Renew, not a second Arm");
    }

    /// An expired lease is never rendered as armed.
    #[test]
    fn expired_lease_falls_back_to_ready() {
        let mut inp = enrolled_inputs();
        inp.armed = true;
        inp.armed_expires_in_ms = Some(0);
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::Ready);
        assert_eq!(st.reason, Some(TouchIdReason::LeaseExpired));
        assert!(st.can_arm && st.can_disarm);
    }

    #[test]
    fn open_ceremony_keeps_disarm_available() {
        let mut inp = enrolled_inputs();
        inp.ceremony_open = true;
        inp.stage_expires_in_ms = Some(90_000);
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::CeremonyOpen);
        assert_eq!(st.stage_expires_in_ms, Some(90_000));
        assert!(st.can_disarm, "disarm clears an open stage");
    }

    #[test]
    fn rehearsal_only_build_is_labelled_but_still_offered() {
        let mut inp = enrolled_inputs();
        inp.allow_real_arm = false;
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::Ready);
        assert_eq!(st.reason, Some(TouchIdReason::RehearsalOnlyBuild));
        assert!(!st.allow_real_arm);
    }

    /// Nothing in the renderer projection can carry ceremony material.
    #[test]
    fn status_projection_carries_no_secret_fields() {
        let mut inp = enrolled_inputs();
        inp.armed = true;
        inp.armed_expires_in_ms = Some(1000);
        let json = serde_json::to_string(&derive_status(&inp)).unwrap();
        for forbidden in [
            "challenge",
            "signature",
            "credential_id",
            "public_key",
            "keyset_hash",
            "token",
            "principal",
            "attestation",
            "authenticator_data",
            "helper_sha256",
            "private",
        ] {
            assert!(
                !json.contains(forbidden),
                "renderer projection must not carry {forbidden}: {json}"
            );
        }
    }

    #[test]
    fn registry_validation_is_fail_closed() {
        assert!(validate_registry(&[]).is_err());

        let mut bad_type = cred("aa00");
        bad_type.credential_type = "password".into();
        assert!(validate_registry(&[bad_type]).is_err());

        let mut bad_key = cred("aa00");
        bad_key.public_key = "not base64!!".into();
        assert!(validate_registry(&[bad_key]).is_err());

        let mut short_key = cred("aa00");
        short_key.public_key = "AAAA".into(); // 3 bytes, not a SEC1 point
        assert!(validate_registry(&[short_key]).is_err());

        let mut ctrl = cred("aa00");
        ctrl.label = "line\nbreak".into();
        assert!(validate_registry(&[ctrl]).is_err());

        assert!(validate_registry(&[cred("aa00"), cred("aa00")]).is_err());
        assert!(validate_registry(&[cred("aa00"), cred("bb11")]).is_ok());
    }

    /// The digest must be order-independent and stable — it is the continuity
    /// proof compared against the running sidecar.
    #[test]
    fn keyset_hash_is_order_independent_and_stable() {
        let a = registry_keyset_hash(&[cred("aa00"), cred("bb11")]).unwrap();
        let b = registry_keyset_hash(&[cred("bb11"), cred("aa00")]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, registry_keyset_hash(&[cred("aa00")]).unwrap());
    }

    /// Canonical bytes are compact with sorted keys — the JCS shape the sidecar
    /// hashes.
    #[test]
    fn canonical_registry_bytes_are_sorted_and_compact() {
        let bytes = canonical_registry_bytes(&[cred("bb11"), cred("aa00")]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(' '), "canonical form has no whitespace");
        let first_id = text.find("aa00").unwrap();
        let second_id = text.find("bb11").unwrap();
        assert!(first_id < second_id, "records sort by credential_id");
        // Object keys are emitted in sorted order:
        // credential_id < credential_type < enrolled_at < label < public_key.
        let id_at = text.find("\"credential_id\"").unwrap();
        let type_at = text.find("\"credential_type\"").unwrap();
        let enrolled_at = text.find("\"enrolled_at\"").unwrap();
        let label_at = text.find("\"label\"").unwrap();
        let pubkey_at = text.find("\"public_key\"").unwrap();
        assert!(
            id_at < type_at
                && type_at < enrolled_at
                && enrolled_at < label_at
                && label_at < pubkey_at,
            "object keys must be sorted: {text}"
        );
    }

    #[test]
    fn malformed_helper_enroll_output_is_refused() {
        assert!(parse_enroll_output("").is_err());
        assert!(parse_enroll_output("not json").is_err());
        assert!(parse_enroll_output(r#"{"credential_id":"aa"}"#).is_err());
        // Two concatenated documents (a compromised helper smuggling bytes).
        let one = serde_json::to_string(&cred("aa00")).unwrap();
        assert!(parse_enroll_output(&format!("{one}{one}")).is_err());
        // A FIDO2 record is not a Touch ID enrollment.
        let mut fido = cred("aa00");
        fido.credential_type = "fido2-es256".into();
        assert!(parse_enroll_output(&serde_json::to_string(&fido).unwrap()).is_err());
        assert!(parse_enroll_output(&one).is_ok());
    }

    #[test]
    fn helper_signature_must_bind_to_the_enrolled_credential() {
        let good = serde_json::json!({
            "credential_id": "aa00",
            "credential_type": "se-p256",
            "signature": "AAAA",
            "authenticator_data": null,
        })
        .to_string();
        assert!(parse_sign_output(&good, "aa00").is_ok());
        assert!(
            parse_sign_output(&good, "bb11").is_err(),
            "a signature from an unenrolled credential must never reach confirm"
        );

        let wrong_type = serde_json::json!({
            "credential_id": "aa00",
            "credential_type": "fido2-es256",
            "signature": "AAAA",
        })
        .to_string();
        assert!(parse_sign_output(&wrong_type, "aa00").is_err());

        let bad_sig = serde_json::json!({
            "credential_id": "aa00",
            "credential_type": "se-p256",
            "signature": "!!!!",
        })
        .to_string();
        assert!(parse_sign_output(&bad_sig, "aa00").is_err());

        assert!(parse_sign_output("", "aa00").is_err());
        assert!(parse_sign_output("garbage", "aa00").is_err());
    }

    #[test]
    fn base64_length_check_rejects_malformed_input() {
        assert_eq!(base64_decoded_len("AAAA"), Some(3));
        assert_eq!(base64_decoded_len("AAA="), Some(2));
        assert_eq!(base64_decoded_len("AA=="), Some(1));
        assert_eq!(base64_decoded_len(""), None);
        assert_eq!(base64_decoded_len("AAA"), None);
        assert_eq!(base64_decoded_len("A=AA"), None);
        assert_eq!(base64_decoded_len("AA A"), None);
    }

    #[test]
    fn registry_container_path_matches_the_pack_pin() {
        assert_eq!(REGISTRY_CONTAINER_PATH, "/run/secrets/arm_attest_keys.json");
    }

    #[test]
    fn reenrollment_archives_are_atomic_moves_not_copies() {
        use std::os::unix::fs::MetadataExt;

        let root = std::env::temp_dir().join(format!(
            "irin-touch-id-archive-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let record = root.join(ENROLLMENT_RECORD_NAME);
        let blob = root.join(ENCLAVE_KEY_NAME);
        std::fs::write(&record, b"public-record").unwrap();
        std::fs::write(&blob, b"opaque-wrapper").unwrap();
        let record_inode = std::fs::metadata(&record).unwrap().ino();
        let blob_inode = std::fs::metadata(&blob).unwrap().ino();

        let archived_record = archive_existing(&record, 1234).unwrap().unwrap();
        let archived_blob = archive_existing(&blob, 1234).unwrap().unwrap();

        assert!(!record.exists() && !blob.exists());
        assert_eq!(
            std::fs::metadata(&archived_record).unwrap().ino(),
            record_inode
        );
        assert_eq!(std::fs::metadata(&archived_blob).unwrap().ino(), blob_inode);
        assert_eq!(std::fs::read(&archived_record).unwrap(), b"public-record");
        assert_eq!(std::fs::read(&archived_blob).unwrap(), b"opaque-wrapper");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_reenrollment_cannot_report_ready() {
        let mut inp = enrolled_inputs();
        // The enrollment record is archived before the opaque wrapper. If the
        // second rename or replacement ceremony stops here, custody is
        // ambiguous and the state remains fail-closed.
        inp.enrollment_record_present = false;
        inp.enclave_key_present = true;
        let st = derive_status(&inp);
        assert_eq!(st.state, TouchIdState::ReenrollRequired);
        assert_eq!(st.reason, Some(TouchIdReason::EnrollmentMissing));
        assert!(!st.can_arm && !st.can_renew);
    }
}
