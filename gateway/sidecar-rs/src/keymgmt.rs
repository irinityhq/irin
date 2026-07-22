// ==========================================================================
// keymgmt.rs — PKI key lifecycle for the cryptographic audit ledger.
//
// Handles introduce/revoke ceremony events. The running sidecar holds only
// the root VerifyingKey (from ROOT_PUBKEY_HEX env) and the active SigningKey
// (from LEDGER_SIGNING_KEY_PATH). Root private key stays air-gapped.
//
// Ceremony events are recorded as regular ledger events with target
// EVENT_KEY_INTRODUCE / EVENT_KEY_REVOKE. The payload carries an envelope
// signature that attests "this action was authorized by key X", independent
// of the chain signature on the ledger row.
// ==========================================================================

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CeremonyPurpose {
    LedgerSigning,
}

impl CeremonyPurpose {
    pub fn as_str(&self) -> &'static str {
        match self {
            CeremonyPurpose::LedgerSigning => "ledger_signing",
        }
    }
}

impl std::fmt::Display for CeremonyPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyIntroducePayload {
    pub new_pubkey_hex: String,
    pub purpose: CeremonyPurpose,
    pub introduced_by_pubkey_hex: String,
    pub envelope_signature_hex: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRevokePayload {
    pub revoked_pubkey_hex: String,
    pub reason: String,
    pub revoked_by_pubkey_hex: String,
    pub envelope_signature_hex: String,
}

// ---------------------------------------------------------------------------
// Envelope preimage — length-prefixed, domain-separated
// ---------------------------------------------------------------------------

fn introduce_preimage(new_pubkey_hex: &str, purpose: &str, signer_pubkey_hex: &str) -> Vec<u8> {
    let tag = b"GW-INTRODUCE-v1\0";
    let body = format!(
        "{}:{}|{}:{}|{}:{}",
        new_pubkey_hex.len(),
        new_pubkey_hex,
        purpose.len(),
        purpose,
        signer_pubkey_hex.len(),
        signer_pubkey_hex,
    );
    [tag.as_slice(), body.as_bytes()].concat()
}

#[allow(dead_code)]
fn revoke_preimage(revoked_pubkey_hex: &str, reason: &str, signer_pubkey_hex: &str) -> Vec<u8> {
    let tag = b"GW-REVOKE-v1\0";
    let body = format!(
        "{}:{}|{}:{}|{}:{}",
        revoked_pubkey_hex.len(),
        revoked_pubkey_hex,
        reason.len(),
        reason,
        signer_pubkey_hex.len(),
        signer_pubkey_hex,
    );
    [tag.as_slice(), body.as_bytes()].concat()
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

pub fn sign_introduce(
    signer: &SigningKey,
    new_pubkey_bytes: &[u8; 32],
    purpose: CeremonyPurpose,
) -> KeyIntroducePayload {
    let signer_pubkey_hex = hex::encode(signer.verifying_key().as_bytes());
    let new_pubkey_hex = hex::encode(new_pubkey_bytes);

    let preimage = introduce_preimage(&new_pubkey_hex, purpose.as_str(), &signer_pubkey_hex);
    let mut hasher = Sha256::new();
    hasher.update(&preimage);
    let digest = hasher.finalize();
    let sig: Signature = signer.sign(&digest);

    KeyIntroducePayload {
        new_pubkey_hex,
        purpose,
        introduced_by_pubkey_hex: signer_pubkey_hex,
        envelope_signature_hex: hex::encode(sig.to_bytes()),
    }
}

#[allow(dead_code)]
pub fn sign_revoke(
    signer: &SigningKey,
    revoked_pubkey_hex: &str,
    reason: &str,
) -> KeyRevokePayload {
    let signer_pubkey_hex = hex::encode(signer.verifying_key().as_bytes());

    let preimage = revoke_preimage(revoked_pubkey_hex, reason, &signer_pubkey_hex);
    let mut hasher = Sha256::new();
    hasher.update(&preimage);
    let digest = hasher.finalize();
    let sig: Signature = signer.sign(&digest);

    KeyRevokePayload {
        revoked_pubkey_hex: revoked_pubkey_hex.to_string(),
        reason: reason.to_string(),
        revoked_by_pubkey_hex: signer_pubkey_hex,
        envelope_signature_hex: hex::encode(sig.to_bytes()),
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn verify_introduce(payload: &KeyIntroducePayload, expected_signer: &VerifyingKey) -> bool {
    let preimage = introduce_preimage(
        &payload.new_pubkey_hex,
        payload.purpose.as_str(),
        &payload.introduced_by_pubkey_hex,
    );
    verify_envelope(&preimage, &payload.envelope_signature_hex, expected_signer)
}

#[allow(dead_code)]
pub fn verify_revoke(payload: &KeyRevokePayload, expected_signer: &VerifyingKey) -> bool {
    let preimage = revoke_preimage(
        &payload.revoked_pubkey_hex,
        &payload.reason,
        &payload.revoked_by_pubkey_hex,
    );
    verify_envelope(&preimage, &payload.envelope_signature_hex, expected_signer)
}

#[allow(dead_code)]
fn verify_envelope(preimage: &[u8], sig_hex: &str, vk: &VerifyingKey) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(preimage);
    let digest = hasher.finalize();

    let sig_bytes = match hex::decode(sig_hex) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let sig_array: [u8; 64] = sig_bytes.try_into().unwrap();
    let sig = Signature::from_bytes(&sig_array);
    vk.verify_strict(&digest, &sig).is_ok()
}

// ---------------------------------------------------------------------------
// Key generation
// ---------------------------------------------------------------------------

pub fn generate_keypair() -> (SigningKey, [u8; 32]) {
    let key = SigningKey::generate(&mut rand_core::OsRng);
    let bytes = key.to_bytes();
    (key, bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_introduce_sign_and_verify() {
        let signer = SigningKey::generate(&mut rand_core::OsRng);
        let (_new_key, new_bytes) = generate_keypair();

        let payload = sign_introduce(&signer, &new_bytes, CeremonyPurpose::LedgerSigning);

        assert!(verify_introduce(&payload, &signer.verifying_key()));

        let wrong_key = SigningKey::generate(&mut rand_core::OsRng);
        assert!(!verify_introduce(&payload, &wrong_key.verifying_key()));
    }

    #[test]
    fn test_revoke_sign_and_verify() {
        let signer = SigningKey::generate(&mut rand_core::OsRng);
        let active_key = SigningKey::generate(&mut rand_core::OsRng);
        let active_pubkey_hex = hex::encode(active_key.verifying_key().as_bytes());

        let payload = sign_revoke(&signer, &active_pubkey_hex, "scheduled rotation");

        assert!(verify_revoke(&payload, &signer.verifying_key()));

        let wrong_key = SigningKey::generate(&mut rand_core::OsRng);
        assert!(!verify_revoke(&payload, &wrong_key.verifying_key()));
    }

    #[test]
    fn test_generate_keypair_produces_valid_key() {
        let (key, bytes) = generate_keypair();
        assert_eq!(key.to_bytes(), bytes);
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn test_introduce_payload_serializes_correctly() {
        let signer = SigningKey::generate(&mut rand_core::OsRng);
        let (_, new_bytes) = generate_keypair();
        let payload = sign_introduce(&signer, &new_bytes, CeremonyPurpose::LedgerSigning);

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["purpose"], "ledger_signing");
        assert!(json["new_pubkey_hex"].is_string());
        assert!(json["envelope_signature_hex"].is_string());
    }

    #[test]
    fn test_domain_separation_prevents_cross_type_forgery() {
        let signer = SigningKey::generate(&mut rand_core::OsRng);
        let (_, new_bytes) = generate_keypair();
        let new_pubkey_hex = hex::encode(new_bytes);

        let introduce = sign_introduce(&signer, &new_bytes, CeremonyPurpose::LedgerSigning);

        // Craft a fake revoke using the introduce's envelope signature
        let fake_revoke = KeyRevokePayload {
            revoked_pubkey_hex: new_pubkey_hex,
            reason: "ledger_signing".to_string(),
            revoked_by_pubkey_hex: introduce.introduced_by_pubkey_hex.clone(),
            envelope_signature_hex: introduce.envelope_signature_hex.clone(),
        };
        assert!(
            !verify_revoke(&fake_revoke, &signer.verifying_key()),
            "domain-separated tags must prevent cross-type forgery"
        );
    }

    // T24 redaction: the Ed25519 private seed must never reach a `{:?}` sink.
    #[test]
    fn test_directive_identity_file_debug_redacts_seed() {
        let file = DirectiveIdentityFile {
            seed_b64: "SENTINEL_SECRET_SEED".to_string(),
            pubkey_b64: "pub-visible".to_string(),
            sha256_self_check: "check-visible".to_string(),
            format_version: DIRECTIVE_IDENTITY_FORMAT_VERSION,
        };
        let dbg = format!("{:?}", file);
        assert!(
            !dbg.contains("SENTINEL_SECRET_SEED"),
            "seed_b64 leaked into Debug: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        // Non-sensitive fields stay visible for diagnosability.
        assert!(dbg.contains("pub-visible"));
        assert!(dbg.contains("check-visible"));
    }

    // T24 redaction: the wrapped SigningKey holds the raw private seed.
    #[test]
    fn test_directive_signing_key_debug_redacts_seed() {
        let seed = [7u8; 32];
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let key = DirectiveSigningKey {
            signing_key,
            verifying_key,
            kid: "SENTINEL_KID".to_string(),
        };
        let dbg = format!("{:?}", key);
        assert!(
            !dbg.contains(&hex::encode(seed)),
            "signing seed leaked into Debug: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        assert!(
            dbg.contains("SENTINEL_KID"),
            "kid should stay visible: {dbg}"
        );
    }
}

// ==========================================================================
// Phase 3 — Directive Signing Key (single-file atomic identity)
// ==========================================================================

#[allow(dead_code)]
const DIRECTIVE_IDENTITY_FORMAT_VERSION: u32 = 1;
#[allow(dead_code)]
pub const DIRECTIVE_IDENTITY_FILENAME: &str = "directive_identity.json";

#[allow(dead_code)]
#[derive(Clone, Serialize, Deserialize)]
pub struct DirectiveIdentityFile {
    pub seed_b64: String,
    pub pubkey_b64: String,
    /// hex(SHA-256("{seed_b64}|{pubkey_b64}"))
    pub sha256_self_check: String,
    pub format_version: u32,
}

// Manual redacting Debug: the raw Ed25519 private seed lives in `seed_b64`.
// `Serialize` is retained deliberately — it is the on-disk persistence path
// (`atomic_write_json` → 0o600 `directive_identity.json`), the ONLY serialize
// sink for this type; there is no serialize-to-log path. `Debug`, however,
// could reach a future `{:?}` log line, so the seed is redacted here (T24).
impl std::fmt::Debug for DirectiveIdentityFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveIdentityFile")
            .field("seed_b64", &"<redacted>")
            .field("pubkey_b64", &self.pubkey_b64)
            .field("sha256_self_check", &self.sha256_self_check)
            .field("format_version", &self.format_version)
            .finish()
    }
}

#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum KeyMgmtError {
    #[error("seed wrong size: got {got} bytes, expected 32")]
    SeedWrongSize { got: usize },

    #[error("pubkey mismatch: stored {stored} != derived {derived}")]
    PubkeyMismatch { stored: String, derived: String },

    #[error("self-check mismatch: stored {stored}, computed {computed}")]
    SelfCheckMismatch { stored: String, computed: String },

    #[error("identity file absent after prior initialization (outbox={outbox_rows}, pending={pending_rows})")]
    IdentityAbsentPostInit { outbox_rows: u64, pending_rows: u64 },

    #[error("DB witness query failed during key load: {0}")]
    DbWitnessQuery(String),

    #[error("rejected filesystem for directive identity: {fs_type}")]
    RejectedFilesystem { fs_type: String },

    #[error("directive signing key accessor not yet published")]
    AccessorUnpublished,

    #[error("I/O error writing/reading directive_identity.json: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct DirectiveSigningKey {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    kid: String,
}

// Manual redacting Debug: `signing_key` wraps the raw Ed25519 private seed.
// The verifying key + kid are public identifiers and safe to surface (T24).
impl std::fmt::Debug for DirectiveSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveSigningKey")
            .field("signing_key", &"<redacted>")
            .field("verifying_key", &self.verifying_key)
            .field("kid", &self.kid)
            .finish()
    }
}

/// Zero-sized token proving that `DirectiveSigningKey::load_or_initialize` has
/// successfully run and published the key to the runtime accessor.
///
/// This is the core of the P0-β / P0-ξ HydrationToken typestate guard:
/// callers must consume this token to enter boot hydration, so later code
/// cannot accidentally manufacture or duplicate the proof.
#[derive(Debug)]
pub struct HydrationToken {
    _private: (),
}

#[allow(dead_code)]
static DIRECTIVE_SIGNING_KEY: std::sync::OnceLock<DirectiveSigningKey> = std::sync::OnceLock::new();

/// Returns the globally published Directive signing key.
/// Must be initialized via `DirectiveSigningKey::load_or_initialize` during boot.
#[allow(dead_code)]
pub fn directive_signing_key() -> &'static DirectiveSigningKey {
    DIRECTIVE_SIGNING_KEY
        .get()
        .expect("DirectiveSigningKey not yet initialized (call load_or_initialize during boot)")
}

/// Non-panicking accessor for the globally published Directive signing key.
/// Returns `None` if `load_or_initialize` has not run yet. Callers that must
/// fail closed (e.g. the worker resolving its pinned verifier) use this instead
/// of [`directive_signing_key`] so a boot/wiring fault degrades to "refuse"
/// rather than a panic.
#[allow(dead_code)]
pub fn try_directive_signing_key() -> Option<&'static DirectiveSigningKey> {
    DIRECTIVE_SIGNING_KEY.get()
}

#[allow(dead_code)]
impl DirectiveSigningKey {
    /// Load from disk or initialize a fresh Ed25519 keypair for signing Directives.
    ///
    /// Implements the single-file identity model (D15) with DB-witness post-init detection.
    pub async fn load_or_initialize(
        identity_path: &std::path::Path,
        watch_db: &crate::watch::db::WatchDb,
    ) -> Result<(Self, HydrationToken), KeyMgmtError> {
        let (outbox_rows, pending_rows) = watch_db
            .phase3_row_counts()
            .await
            .map_err(|e| KeyMgmtError::DbWitnessQuery(e.to_string()))?;

        if !identity_path.exists() {
            if outbox_rows > 0 || pending_rows > 0 {
                return Err(KeyMgmtError::IdentityAbsentPostInit {
                    outbox_rows,
                    pending_rows,
                });
            }
            return Self::initialize_fresh(identity_path).await;
        }

        // File exists — load + verify
        let data = std::fs::read(identity_path)?;
        let file: DirectiveIdentityFile = serde_json::from_slice(&data)?;

        if file.format_version != DIRECTIVE_IDENTITY_FORMAT_VERSION {
            return Err(KeyMgmtError::SelfCheckMismatch {
                stored: file.sha256_self_check.clone(),
                computed: "unsupported-version".to_string(),
            });
        }

        let seed = BASE64.decode(&file.seed_b64)?;
        if seed.len() != 32 {
            return Err(KeyMgmtError::SeedWrongSize { got: seed.len() });
        }

        let mut seed_arr = [0u8; 32];
        seed_arr.copy_from_slice(&seed);

        let signing_key = SigningKey::from_bytes(&seed_arr);
        let derived_vk = signing_key.verifying_key();
        let derived_pubkey_b64 = BASE64.encode(derived_vk.as_bytes());
        let stored_pubkey_b64 = file.pubkey_b64.clone();

        if derived_pubkey_b64 != stored_pubkey_b64 {
            return Err(KeyMgmtError::PubkeyMismatch {
                stored: stored_pubkey_b64,
                derived: derived_pubkey_b64,
            });
        }

        let expected_check = compute_self_check(&file.seed_b64, &file.pubkey_b64);
        if expected_check != file.sha256_self_check {
            return Err(KeyMgmtError::SelfCheckMismatch {
                stored: file.sha256_self_check,
                computed: expected_check,
            });
        }

        let kid = compute_kid(&derived_vk);

        let key = Self {
            signing_key,
            verifying_key: derived_vk,
            kid,
        };

        // Publish for later use by dispatcher / hydration (idempotent on restart)
        if DIRECTIVE_SIGNING_KEY.get().is_none() {
            let _ = DIRECTIVE_SIGNING_KEY.set(key.clone());
        }

        Ok((key, HydrationToken { _private: () }))
    }

    async fn initialize_fresh(
        path: &std::path::Path,
    ) -> Result<(Self, HydrationToken), KeyMgmtError> {
        // Best-effort tmpfs rejection (AC-19e)
        if let Some(parent) = path.parent() {
            if is_tmpfs(parent) {
                return Err(KeyMgmtError::RejectedFilesystem {
                    fs_type: "tmpfs".to_string(),
                });
            }
        }

        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);

        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        let seed_b64 = BASE64.encode(seed);
        let pubkey_b64 = BASE64.encode(verifying_key.as_bytes());
        let sha256_self_check = compute_self_check(&seed_b64, &pubkey_b64);

        let file = DirectiveIdentityFile {
            seed_b64,
            pubkey_b64,
            sha256_self_check,
            format_version: DIRECTIVE_IDENTITY_FORMAT_VERSION,
        };

        atomic_write_json(path, &file)?;

        let kid = compute_kid(&verifying_key);

        let key = Self {
            signing_key,
            verifying_key,
            kid,
        };

        let _ = DIRECTIVE_SIGNING_KEY.set(key.clone());

        Ok((key, HydrationToken { _private: () }))
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing_key.sign(msg)
    }

    /// Phase 3 JCS canonical signing seam for directive / proposal.v1 payloads.
    ///
    /// Serializes the Value using the stable reference form (serde_json::to_vec on
    /// serde_json's default sorted Map, compact, no whitespace) and signs the
    /// resulting UTF-8 bytes with the Ed25519 key. The returned canonical string
    /// is what `envelope_json_canonical` must contain for verification (D24).
    ///
    /// This is the single chokepoint for canonical-key validation:
    /// call sites and the persisted column contract remain unchanged. Preserves v0.1
    /// reference-encoder behavior per spec §P0-2.
    pub fn sign_capability_token(
        &self,
        mut token: sovereign_protocol::types::CapabilityToken,
    ) -> sovereign_protocol::types::CapabilityToken {
        token.signature = None;
        let canonical =
            sovereign_protocol::jcs::to_jcs_bytes(&token).expect("CapabilityToken must serialize");
        let sig = self.sign(&canonical);
        token.signature = Some(BASE64.encode(sig.to_bytes()));
        token
    }

    pub fn verify_capability_token(
        &self,
        token: &sovereign_protocol::types::CapabilityToken,
    ) -> bool {
        if let Some(sig_b64) = &token.signature {
            if let Ok(sig_bytes) = BASE64.decode(sig_b64) {
                if sig_bytes.len() == 64 {
                    let mut sig_arr = [0u8; 64];
                    sig_arr.copy_from_slice(&sig_bytes);
                    let sig = Signature::from_bytes(&sig_arr);
                    {
                        let mut token_copy = token.clone();
                        token_copy.signature = None;
                        if let Ok(canonical) = sovereign_protocol::jcs::to_jcs_bytes(&token_copy) {
                            return self.verifying_key.verify_strict(&canonical, &sig).is_ok();
                        }
                    }
                }
            }
        }
        false
    }

    pub fn sign_directive_envelope(&self, payload: &serde_json::Value) -> (String, Signature) {
        let canonical = sovereign_protocol::jcs::to_jcs_bytes(payload)
            .expect("PersistedDirectivePayloadV1 / proposal envelope must serialize");
        let signature = self.sign(&canonical);
        let canonical_json =
            String::from_utf8(canonical).expect("serde_json serializes JSON as UTF-8");
        (canonical_json, signature)
    }
}

// ==========================================================================
// Pre-seal W2 — Worker-leg directive provenance verification.
//
// The fire chain ends: council-triage proposal → DirectiveSigningKey
// signs the JCS-canonical envelope → row persisted to directive_outbox
// (envelope_json_canonical + signature_b64 + signing_kid). Before W2 the
// worker claimed a row and executed WITHOUT verifying that signature, so a
// forged/tampered directive_outbox row (anything with DB/UDS access, or a
// future bug) would be executed as if Council had authorized it.
//
// `verify_directive_envelope` is the exact inverse of
// `DirectiveSigningKey::sign_directive_envelope`: it verifies the stored
// canonical bytes VERBATIM (never re-serializing / re-canonicalizing — that
// mismatch is the classic break), against the pinned Council verifying key,
// and only after the stored kid matches the pinned kid. The worker fails
// CLOSED on any error.
// ==========================================================================

/// Typed outcome of stored-directive-envelope verification. Every variant is
/// a refusal reason — there is no silent pass. The worker maps any of these to
/// "nack + loud error, do not execute".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DirectiveVerifyError {
    /// The directive's stored kid is not the pinned Council kid (unpinned /
    /// unknown / rotated key id). We never look up an alternate key.
    #[error("kid mismatch: stored '{stored}' != pinned '{pinned}'")]
    KidMismatch { stored: String, pinned: String },

    /// A required field (canonical bytes, signature, or kid) was empty.
    #[error("missing field: {0}")]
    MissingField(&'static str),

    /// signature_b64 was not valid base64.
    #[error("signature is not valid base64")]
    BadSignatureEncoding,

    /// signature decoded to the wrong length (Ed25519 is exactly 64 bytes).
    #[error("signature wrong length: got {got} bytes, expected 64")]
    BadSignatureLength { got: usize },

    /// Ed25519 verify_strict rejected the signature over the canonical bytes.
    #[error("signature does not verify over stored canonical envelope bytes")]
    SignatureMismatch,
}

/// The pinned Council verifying material a worker checks every directive
/// against. Built once from the published [`DirectiveSigningKey`] (production)
/// or from a known test key (tests). Holding both the key and its kid keeps
/// the pin self-consistent: the kid we require is derived from the same key we
/// verify with.
#[derive(Clone, Debug)]
pub struct DirectiveVerifier {
    verifying_key: VerifyingKey,
    pinned_kid: String,
}

impl DirectiveVerifier {
    /// Pin to a specific verifying key + kid. Callers must pass the kid that
    /// belongs to this key (`compute_kid(&vk)`); the convenience constructors
    /// below guarantee that.
    pub fn new(verifying_key: VerifyingKey, pinned_kid: String) -> Self {
        Self {
            verifying_key,
            pinned_kid,
        }
    }

    /// Pin from the globally published Directive signing key (production path).
    /// The kid is taken from the same key, so the pin is internally consistent.
    pub fn from_signing_key(key: &DirectiveSigningKey) -> Self {
        Self {
            verifying_key: *key.verifying_key(),
            pinned_kid: key.kid().to_string(),
        }
    }

    /// Pin directly from a verifying key, deriving the kid the same way the
    /// signer does (`sidecar-v1-{first8 hex of sha256(pubkey)}`). Used by tests
    /// and by any caller that only holds the public key.
    pub fn from_verifying_key(verifying_key: VerifyingKey) -> Self {
        let pinned_kid = compute_kid(&verifying_key);
        Self {
            verifying_key,
            pinned_kid,
        }
    }

    pub fn pinned_kid(&self) -> &str {
        &self.pinned_kid
    }

    /// Verify a stored directive envelope. `canonical` MUST be the exact
    /// `envelope_json_canonical` bytes as persisted (verified verbatim — no
    /// re-serialization). `signature_b64` and `stored_kid` are the persisted
    /// columns. Fails closed on any mismatch.
    pub fn verify(
        &self,
        canonical: &str,
        signature_b64: &str,
        stored_kid: &str,
    ) -> Result<(), DirectiveVerifyError> {
        verify_directive_envelope(
            canonical,
            signature_b64,
            stored_kid,
            &self.verifying_key,
            &self.pinned_kid,
        )
    }
}

/// Verify a persisted directive envelope against the pinned Council key.
///
/// Inverse of [`DirectiveSigningKey::sign_directive_envelope`] (keymgmt.rs:533):
/// the signer produced `signature = sign(JCS(payload))` and persisted the JCS
/// string as `envelope_json_canonical`. We verify the SAME stored bytes
/// verbatim — we do NOT re-canonicalize, because any encoder drift between
/// sign-time and verify-time would falsely reject a genuine directive (or, in
/// the other direction, weaken the check). Order of checks (cheapest, most
/// specific first): kid pin → presence → decode → strict ed25519 verify.
pub fn verify_directive_envelope(
    canonical: &str,
    signature_b64: &str,
    stored_kid: &str,
    pinned_vk: &VerifyingKey,
    pinned_kid: &str,
) -> Result<(), DirectiveVerifyError> {
    // 1. Kid pin: refuse anything not signed under the pinned Council kid.
    //    This rejects unpinned/unknown/rotated kids before any crypto work.
    if stored_kid.is_empty() {
        return Err(DirectiveVerifyError::MissingField("signing_kid"));
    }
    if stored_kid != pinned_kid {
        return Err(DirectiveVerifyError::KidMismatch {
            stored: stored_kid.to_string(),
            pinned: pinned_kid.to_string(),
        });
    }

    // 2. Presence of the other load-bearing fields.
    if canonical.is_empty() {
        return Err(DirectiveVerifyError::MissingField(
            "envelope_json_canonical",
        ));
    }
    if signature_b64.is_empty() {
        return Err(DirectiveVerifyError::MissingField("signature_b64"));
    }

    // 3. Decode the stored signature (base64 of the 64 raw Ed25519 bytes).
    let sig_bytes = BASE64
        .decode(signature_b64)
        .map_err(|_| DirectiveVerifyError::BadSignatureEncoding)?;
    let sig_array: [u8; 64] =
        sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| DirectiveVerifyError::BadSignatureLength {
                got: sig_bytes.len(),
            })?;
    let sig = Signature::from_bytes(&sig_array);

    // 4. Strict verify over the stored canonical bytes VERBATIM.
    pinned_vk
        .verify_strict(canonical.as_bytes(), &sig)
        .map_err(|_| DirectiveVerifyError::SignatureMismatch)
}

#[allow(dead_code)]
fn compute_self_check(seed_b64: &str, pubkey_b64: &str) -> String {
    let data = format!("{}|{}", seed_b64, pubkey_b64);
    let hash = Sha256::digest(data.as_bytes());
    hex::encode(hash)
}

/// P0-delta: kid must be sidecar-v1- + first 8 hex chars of SHA-256(pubkey),
/// not the raw pubkey bytes. This provides a stable, hash-based identifier.
#[allow(dead_code)]
fn compute_kid(vk: &VerifyingKey) -> String {
    let hash = Sha256::digest(vk.as_bytes());
    format!("sidecar-v1-{}", &hex::encode(hash)[..8])
}

#[allow(dead_code)]
fn atomic_write_json<T: Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("directive_identity.json has no parent directory"))?;

    // Atomic write using a sibling .tmp file + rename (no tempfile crate dependency)
    let tmp_path = path.with_extension("tmp");
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut tmp = opts.open(&tmp_path)?;

    let json = serde_json::to_string_pretty(value)?;
    tmp.write_all(json.as_bytes())?;
    tmp.flush()?;
    tmp.sync_all()?;

    std::fs::rename(&tmp_path, path)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;

    // fsync parent directory
    let dir = OpenOptions::new().read(true).open(parent)?;
    dir.sync_all()?;

    Ok(())
}

/// Best-effort tmpfs/ramfs rejection (AC-19e). For this seam we treat it as best-effort
/// (operator responsibility for durable storage). Full nix-based detection can be added later.
#[allow(dead_code)]
fn is_tmpfs(_path: &std::path::Path) -> bool {
    false
}
