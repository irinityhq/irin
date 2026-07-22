//! Local attestation for the arm-confirm challenge. Canonical JCS bytes are
//! built once at stage time,
//! stored verbatim in `arm_pending.challenge_bytes`, served base64 over
//! `GET /arm/pending`, signed by the host helper as opaque bytes, and (B4)
//! verified against the STORED bytes — never a re-derivation. The helper
//! never parses or re-serializes JSON, so cross-language JCS drift is
//! structurally impossible on the primary path.
//!
//! `v` is the challenge format version: any change to
//! the challenge shape bumps it, and [`challenge_format_self_test`] runs at
//! boot — round-trip a fixed test vector (canonicalize → digest → compare
//! against pinned bytes) — so serialization drift fails loudly at boot,
//! never at arm time. All timestamps are integer milliseconds (no floats
//! anywhere near JCS).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Challenge format version. Bump on any shape change and re-pin
/// [`PINNED_VECTOR_SHA256`]. Version 2 added content-binding fields
/// (`build_id`, `enabled_surface`,
/// `effective_daily_cap_cents`, `tenant`) entered the signed challenge so the
/// tap binds to specific INTENT, not an opaque stage_id. v3 (Attested-arm,
/// invariant): `spend_window_ms` enters the signed
/// challenge so the SPEND HORIZON is anchored to the tap, not to an
/// attacker-restartable `GW_ARM_WINDOW_MS` boot knob — the last unsigned
/// spend-gate input (the cap was already signed). A rolled-back v2 binary
/// emits/expects v2 and the boot self-test rejects v3 cleanly (and the reserve
/// floor below requires `v >= 3`) — downgrade is fail-closed.
pub const CHALLENGE_FORMAT_VERSION: u32 = 3;

/// The current surface taxonomy has a single
/// value: a confirmed tap authorizes the WATCH PRODUCER and nothing else.
/// worker-execute / capability-token surfaces are v2 (fresh gate each); this
/// is wired forward-compat, not a live multi-surface gate.
pub const ENABLED_SURFACE_WATCH_PRODUCER: &str = "watch-producer";

/// The current challenge is single-tenant. The `tenant` field keeps the
/// signed shape ready for a future per-tenant gate.
pub const CANARY_TENANT: &str = "canary";

/// Fixed `kind` discriminator — a signature over these bytes can never be
/// confused with any other signed artifact in the system.
pub const CHALLENGE_KIND: &str = "arm-confirm-challenge";

/// RIDER D tag, pinned INSIDE the signed challenge (independent of the
/// `GW_ARM_DEVIATION_FLAG` env value, which feeds audit detail strings).
pub const CHALLENGE_DEVIATION_TAG: &str = "dual-custody-local-attest";

/// SHA-256 (hex) of the JCS canonical bytes of [`pinned_test_vector`].
/// Recompute only on a deliberate format-version bump:
/// `printf '%s' '<canonical json>' | shasum -a 256`.
/// Version 2 covers the four content-binding fields; version 3 adds the
/// signed `spend_window_ms`.
const PINNED_VECTOR_SHA256: &str =
    "6d16bab2218abb0f9397dd294f0da6ddbd736289a788be2caa3d72b9d6cd4188";

/// The arm-confirm challenge (spec §5). Serialized through
/// `sovereign_protocol::jcs` (RFC 8785: sorted keys, no whitespace) exactly
/// once, at stage time. `stage_id` embeds the staged-to-approved binding:
/// a signature over this challenge is unusable for any
/// other stage, any other deployment epoch (nonce), or after expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmConfirmChallenge {
    pub v: u32,
    pub kind: String,
    pub stage_id: String,
    pub staged_by: String,
    /// base64 (STANDARD) of 32 CSPRNG bytes.
    pub nonce: String,
    pub iat_ms: i64,
    pub exp_ms: i64,
    /// Attested-arm (v3) — the SPEND-WINDOW horizon, RELATIVE to `iat_ms` (the single
    /// time anchor — an absolute exp would duplicate the deadline and invite
    /// skew). The reserve gates spend on `iat_ms + spend_window_ms` reading THIS
    /// signed value, not the restartable `GW_ARM_WINDOW_MS` boot knob, so a
    /// box-owning attacker cannot extend a genuine tap's window without a fresh
    /// tap. Derived at stage time from the boot-locked window policy (the
    /// `signed.min(ambient)` cap-floor pattern: the signed value equals the boot
    /// policy and can never exceed it).
    pub spend_window_ms: i64,
    pub deviation_tag: String,
    // Content-binding fields.
    /// WHICH code is being armed — embedded build identity (git SHA + `-dirty`,
    /// Q1). A signature over a v2 challenge is unusable against a different
    /// build; confirm compares the EMBEDDED constant, never live git.
    pub build_id: String,
    /// WHAT authority the confirm enables — v1 enum: `"watch-producer"` (Q4).
    pub enabled_surface: String,
    /// HOW MUCH money, in INTEGER CENTS (Q2/B5: no float anywhere near JCS).
    pub effective_daily_cap_cents: i64,
    /// WHOSE — the canary tenant for v1 (carries the multi-tenant seam free).
    pub tenant: String,
}

/// Build the canonical challenge bytes for a freshly staged arm. Called
/// exactly once per stage (B2); the result is stored verbatim and never
/// re-derived. `exp_ms` is the same wall-clock expiry written to
/// `arm_pending.exp_at_ms`.
pub fn build_challenge_bytes(
    stage_id: &str,
    staged_by: &str,
    iat_ms: i64,
    exp_ms: i64,
    content: &ArmContent,
) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    use rand_core::{OsRng, RngCore};
    let mut nonce_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut nonce_bytes);
    let challenge = ArmConfirmChallenge {
        v: CHALLENGE_FORMAT_VERSION,
        kind: CHALLENGE_KIND.to_string(),
        stage_id: stage_id.to_string(),
        staged_by: staged_by.to_string(),
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        iat_ms,
        exp_ms,
        spend_window_ms: content.effective_spend_window_ms,
        deviation_tag: CHALLENGE_DEVIATION_TAG.to_string(),
        build_id: content.build_id.clone(),
        enabled_surface: content.enabled_surface.clone(),
        effective_daily_cap_cents: content.effective_daily_cap_cents,
        tenant: content.tenant.clone(),
    };
    sovereign_protocol::jcs::to_jcs_bytes(&challenge)
        .map_err(|e| anyhow::anyhow!("challenge JCS canonicalization failed: {e}"))
}

/// The embedded build identity:
/// `"<sha>"` for a clean tree, `"<sha>-dirty"` for an uncommitted one. Read
/// from the `env!()` constants `build.rs` baked in at compile time — NEVER
/// live `git` at runtime, so stage and confirm in the same binary always agree
/// and a rolled-back binary carries its OWN (different) id. A `-dirty` suffix
/// forces DARK/rehearsal-only arming (B6).
pub fn build_id() -> String {
    // `option_env!` (NOT `env!`): a build environment that never ran build.rs's
    // emit — e.g. a Docker layer with no `.git` / no `git` — must DEGRADE to an
    // unidentifiable, DARK-only build, never HARD-FAIL the compile. `env!` would
    // do the latter and defeat the whole fail-closed intent (B6). Absent var ⇒
    // "unknown" + dirty.
    let sha = build_sha();
    if build_is_dirty() {
        format!("{sha}-dirty")
    } else {
        sha.to_string()
    }
}

/// Full Git commit embedded by `build.rs`. An unavailable identity is the
/// fail-closed value `"unknown"`; callers must never substitute live Git state.
pub fn build_sha() -> &'static str {
    option_env!("GW_BUILD_GIT_SHA").unwrap_or("unknown")
}

/// B6 — true when this binary cannot arm for REAL spend: an unclean (`-dirty`)
/// or unidentifiable build may only stage/confirm as rehearsal/DARK. Absent
/// embedded flag ⇒ treated as dirty (fail-closed), so a build that did not run
/// build.rs's emit can only ever arm DARK.
pub fn build_is_dirty() -> bool {
    option_env!("GW_BUILD_DIRTY")
        .map(|v| v == "true")
        .unwrap_or(true)
}

/// T1 MF-1 (B5) — the 4 content-binding values, derived ONCE and identically
/// by BOTH stage and confirm. The whole point of a single derivation site is
/// that confirm re-derives the SAME values from the SAME inputs; any drift in
/// the ambient cap (or a build/surface/tenant mismatch) between stage and
/// confirm then trips the strict-equality check at confirm time.
///
/// Inputs are explicit (not read here) so the derivation is pure and testable:
/// `build_id` is the embedded constant; `cap_usd` is `daily_spend_cap()` read
/// by the caller; surface and tenant are the v1 constants. Cents conversion is
/// the ONLY float→int step and it happens here, OUTSIDE the JCS-signed path
/// (B5: no float anywhere near the canonicalizer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmContent {
    pub build_id: String,
    pub enabled_surface: String,
    pub effective_daily_cap_cents: i64,
    pub tenant: String,
    /// Attested-arm (v3) — the signed spend-window horizon (ms, relative to the tap
    /// `iat_ms`). Like the cap, it is derived ONCE and identically by stage and
    /// confirm; any drift trips the strict-equality check at confirm time.
    pub effective_spend_window_ms: i64,
}

/// The f64 integer-exact ceiling (2^53). Cents above this cannot round-trip
/// through f64 without precision loss, so we refuse to bind one.
pub const MAX_ARM_CAP_CENTS_F64: f64 = 9_007_199_254_740_992.0;

/// Derive the content-binding tuple. `cap_usd` is the boot-resolved daily
/// spend cap (`db::daily_spend_cap()`); it is converted to integer cents with
/// rounding so the signed path never carries a float.
///
/// Float-saturation guard: `f64 as i64`
/// SATURATES in Rust rather than panicking — `f64::INFINITY` casts to
/// `i64::MAX` (which would silently BIND A CAP far above any real one, defeating
/// the very value MF-1 protects) and `NaN` casts to `0`. So we validate the
/// rounded cents are finite, non-negative, and within the f64 integer-exact
/// range BEFORE the only float→int cast, and fail closed with a real reason
/// otherwise. The boot-locked `daily_spend_cap()` OnceLock should never be
/// non-finite, but the binding site must not depend on the parser for that.
/// `spend_window_ms` (Attested-arm) is the boot-locked window policy
/// (`db::arm_window_ms_bootlocked()`) passed by the caller — the value that gets
/// SIGNED so the reserve can anchor the spend horizon to the tap. It is the
/// cap-floor pattern applied to time: the signed window equals the boot policy
/// (so it can never exceed it), and a post-tap `GW_ARM_WINDOW_MS` restart cannot
/// change what was already signed. A negative window is refused fail-closed.
pub fn derive_arm_content(cap_usd: f64, spend_window_ms: i64) -> Result<ArmContent, String> {
    let cents_f = (cap_usd * 100.0).round();
    // `contains` is false for NaN and +/-INFINITY (no float compares in-range),
    // so this one check covers non-finite, negative, AND over-range in one shot.
    if !(0.0..=MAX_ARM_CAP_CENTS_F64).contains(&cents_f) {
        return Err("invalid_spend_cap".to_string());
    }
    if spend_window_ms < 0 {
        return Err("invalid_spend_window".to_string());
    }
    Ok(ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: cents_f as i64,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: spend_window_ms,
    })
}

/// The fixed self-test vector: every field deterministic (all-zero nonce,
/// zero timestamps) so the canonical bytes — and their digest — are pinned.
fn pinned_test_vector() -> ArmConfirmChallenge {
    ArmConfirmChallenge {
        v: CHALLENGE_FORMAT_VERSION,
        kind: CHALLENGE_KIND.to_string(),
        stage_id: "00000000000000000000000000000000".to_string(),
        staged_by: "test-vector".to_string(),
        // base64 of 32 zero bytes.
        nonce: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
        iat_ms: 0,
        exp_ms: 120_000,
        // v3 signed spend-window — fixed deterministic value (24h) so the
        // canonical bytes (and their digest) stay pinned.
        spend_window_ms: 86_400_000,
        deviation_tag: CHALLENGE_DEVIATION_TAG.to_string(),
        // v2 content-binding fields — fixed deterministic values so the
        // canonical bytes (and their digest) stay pinned. `build_id` is a
        // literal here (NOT `build_id()`) so the self-test is independent of
        // the actual build SHA.
        build_id: "test-vector-build".to_string(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 5000,
        tenant: CANARY_TENANT.to_string(),
    }
}

/// Inner self-test, parameterized on the pinned digest so the corrupted-
/// vector failure mode is itself testable (B2 acceptance: "corrupted vector
/// fails boot loudly").
fn challenge_format_self_test_against(pinned_sha256_hex: &str) -> anyhow::Result<()> {
    let bytes = sovereign_protocol::jcs::to_jcs_bytes(&pinned_test_vector())
        .map_err(|e| anyhow::anyhow!("self-test canonicalization failed: {e}"))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != pinned_sha256_hex {
        anyhow::bail!(
            "arm-confirm challenge format drift: pinned test vector digests to {digest}, \
             expected {pinned_sha256_hex} (format version {CHALLENGE_FORMAT_VERSION}). \
             The challenge serialization no longer matches the pinned format — \
             refusing to boot (challenge-format invariant). Canonical bytes: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    Ok(())
}

/// Boot self-test: canonicalize the pinned
/// test vector, digest it, and compare against the pinned hash. main.rs
/// treats Err as FATAL — serialization drift fails at boot, never at arm
/// time.
pub fn challenge_format_self_test() -> anyhow::Result<()> {
    challenge_format_self_test_against(PINNED_VECTOR_SHA256)
}

// ---------------------------------------------------------------------------
// Enrolled-credential registry. Loaded only at boot, fail-closed exactly
// like `ArmPrincipals`: any violation unloads the ENTIRE registry and confirm
// rejects every attempt with `registry_unloaded`. No runtime mutation API
// exists — no self-service re-enroll by construction; changing the keyset
// requires a host file write + sidecar restart, both of which alert and chain
// into the audit. The real control is DETECTION (keyset hash in the boot
// audit row + ntfy), not the file mode.
// ---------------------------------------------------------------------------

/// Secure-Enclave P-256 credential (primary — Touch ID Macs).
pub const CREDENTIAL_TYPE_SE_P256: &str = "se-p256";
/// External FIDO2 key credential (backup — `libfido2` over USB).
pub const CREDENTIAL_TYPE_FIDO2_ES256: &str = "fido2-es256";

/// One enrolled credential record, as emitted by `bin/arm-enroll` (spec
/// §7.1). `public_key` is base64 SEC1 (33-byte compressed or 65-byte
/// uncompressed P-256 point); `credential_id` is derived from the key
/// (SHA-256 truncated), so it carries no secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestCredential {
    pub credential_id: String,
    pub credential_type: String,
    pub public_key: String,
    pub label: String,
    pub enrolled_at: String,
}

/// The boot-loaded registry. `creds` is sorted by `credential_id` so the
/// keyset hash is order-independent of the file. An UNLOADED registry
/// (`loaded == false`) is the fail-closed state: B4's confirm path rejects
/// everything with `registry_unloaded`.
pub struct AttestKeyRegistry {
    creds: Vec<AttestCredential>,
    keyset_hash: Option<String>,
}

impl AttestKeyRegistry {
    /// The fail-closed empty registry (parse failure / missing file / any
    /// rule violation). Confirm always rejects against it.
    pub fn unloaded() -> Self {
        Self {
            creds: Vec::new(),
            keyset_hash: None,
        }
    }

    /// Fail-closed parse of the registry JSON (an array of
    /// [`AttestCredential`]). ANY violation → the ENTIRE registry is
    /// unloaded (same posture as `ArmPrincipals::parse`):
    ///   * malformed JSON / wrong shape,
    ///   * empty array,
    ///   * duplicate `credential_id`,
    ///   * unknown `credential_type`,
    ///   * `public_key` not base64 of a 33- or 65-byte SEC1 point,
    ///   * no hardware credential left (a registry cannot become single-custody
    ///     by subtraction: removing
    ///     the last hardware credential bricks the gate loudly; with only
    ///     hardware types defined this is implied by non-emptiness, but the
    ///     check is explicit so a future credential type cannot silently
    ///     weaken it).
    pub fn parse(raw: &str) -> Self {
        use base64::Engine as _;
        let mut creds: Vec<AttestCredential> = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "attest key registry: JSON parse failed — registry UNLOADED (fail-closed)");
                return Self::unloaded();
            }
        };
        if creds.is_empty() {
            tracing::error!(
                "attest key registry: empty credential array — registry UNLOADED (fail-closed)"
            );
            return Self::unloaded();
        }
        for c in &creds {
            if c.credential_type != CREDENTIAL_TYPE_SE_P256
                && c.credential_type != CREDENTIAL_TYPE_FIDO2_ES256
            {
                tracing::error!(
                    credential_id = %c.credential_id,
                    credential_type = %c.credential_type,
                    "attest key registry: unknown credential_type — registry UNLOADED (fail-closed)"
                );
                return Self::unloaded();
            }
            match base64::engine::general_purpose::STANDARD.decode(&c.public_key) {
                Ok(pk) if pk.len() == 33 || pk.len() == 65 => {}
                Ok(pk) => {
                    tracing::error!(
                        credential_id = %c.credential_id,
                        len = pk.len(),
                        "attest key registry: public_key is not a 33/65-byte SEC1 point — registry UNLOADED (fail-closed)"
                    );
                    return Self::unloaded();
                }
                Err(e) => {
                    tracing::error!(
                        credential_id = %c.credential_id,
                        error = %e,
                        "attest key registry: public_key is not valid base64 — registry UNLOADED (fail-closed)"
                    );
                    return Self::unloaded();
                }
            }
        }
        creds.sort_by(|a, b| a.credential_id.cmp(&b.credential_id));
        if creds
            .windows(2)
            .any(|w| w[0].credential_id == w[1].credential_id)
        {
            tracing::error!(
                "attest key registry: duplicate credential_id — registry UNLOADED (fail-closed)"
            );
            return Self::unloaded();
        }
        // Every credential has already passed the credential_type check above,
        // so today's valid set is all hardware. Keep this explicit guard as the
        // fail-closed tripwire if a future non-hardware type is admitted.
        let hardware = creds.iter().any(|c| {
            c.credential_type == CREDENTIAL_TYPE_SE_P256
                || c.credential_type == CREDENTIAL_TYPE_FIDO2_ES256
        });
        if !hardware {
            tracing::error!(
                "attest key registry: no hardware credential — registry UNLOADED (fail-closed, 1aba8e1d-445 action 4)"
            );
            return Self::unloaded();
        }
        // Keyset hash (spec §7.2): SHA-256 over JCS of the SORTED records.
        // Operator cross-check: `jq -cS 'sort_by(.credential_id)' <file> |
        // tr -d '\n' | shasum -a 256`.
        let keyset_hash = match sovereign_protocol::jcs::to_jcs_bytes(&creds) {
            Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
            Err(e) => {
                tracing::error!(error = %e, "attest key registry: keyset-hash canonicalization failed — registry UNLOADED (fail-closed)");
                return Self::unloaded();
            }
        };
        Self {
            creds,
            keyset_hash: Some(keyset_hash),
        }
    }

    /// Boot-only load from `GW_ARM_ATTEST_KEYS_PATH`. Unset/empty env or an
    /// unreadable file → unloaded (fail-closed, loudly). There is no reload
    /// path: a keyset change requires a sidecar restart by design.
    pub fn from_env() -> Self {
        let Some(path) = std::env::var("GW_ARM_ATTEST_KEYS_PATH")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            tracing::warn!(
                "GW_ARM_ATTEST_KEYS_PATH unset — attest key registry UNLOADED; \
                 local-attest confirm will reject (registry_unloaded) until enrollment (spec §7)"
            );
            return Self::unloaded();
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let reg = Self::parse(&raw);
                if reg.is_loaded() {
                    tracing::info!(
                        path = %path,
                        credentials = reg.creds.len(),
                        keyset_hash = %reg.keyset_hash.as_deref().unwrap_or(""),
                        "attest key registry loaded (boot-only, fail-closed)"
                    );
                }
                reg
            }
            Err(e) => {
                tracing::error!(
                    path = %path, error = %e,
                    "GW_ARM_ATTEST_KEYS_PATH unreadable — attest key registry UNLOADED (fail-closed)"
                );
                Self::unloaded()
            }
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.keyset_hash.is_some()
    }

    /// Look up an enrolled credential by id (B4 verify path).
    pub fn get(&self, credential_id: &str) -> Option<&AttestCredential> {
        self.creds.iter().find(|c| c.credential_id == credential_id)
    }

    /// SHA-256 (hex) over the JCS of the sorted records; `None` = unloaded.
    pub fn keyset_hash(&self) -> Option<&str> {
        self.keyset_hash.as_deref()
    }

    pub fn len(&self) -> usize {
        self.creds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.creds.is_empty()
    }
}

// ---------------------------------------------------------------------------
// B4 (spec §4.2 + §5) — ES256 verification. DER is the ONLY wire format for
// signatures (`Signature::from_der`; raw r||s fails DER parse — one form, no
// fallback). se-p256 verifies straight over the stored challenge bytes
// (ECDSA-SHA256, matching CryptoKit's `signature(for:)`); fido2-es256
// reconstructs the CTAP composition `authenticatorData || SHA-256(challenge)`
// and additionally checks the UP (user-presence) flag bit and extracts the
// signature counter for strict-increase enforcement (counter logic keys on
// credential_type ONLY — `1aba8e1d-445` action 6).
// ---------------------------------------------------------------------------

/// Successful fido2 verification carries the authenticator's signature
/// counter (big-endian bytes 33..37 of authenticatorData).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fido2Verified {
    pub counter: u32,
}

/// Verify an se-p256 confirm signature: base64-SEC1 enrolled key, DER ES256
/// signature, over the verbatim stored challenge bytes. Error strings are
/// stable audit-reason inputs (`bad_signature` family) — they never carry
/// attacker-controlled bytes.
pub fn verify_se_p256(
    public_key_sec1: &[u8],
    signature_der: &[u8],
    challenge_bytes: &[u8],
) -> Result<(), &'static str> {
    use p256::ecdsa::signature::Verifier;
    let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key_sec1)
        .map_err(|_| "enrolled public key is not a valid SEC1 P-256 point")?;
    let sig = p256::ecdsa::Signature::from_der(signature_der)
        .map_err(|_| "signature is not DER (raw r||s is rejected — spec §5)")?;
    key.verify(challenge_bytes, &sig)
        .map_err(|_| "ES256 verification failed")
}

/// Verify a fido2-es256 confirm assertion (spec §5): the signed message is
/// `authenticatorData || clientDataHash` where `clientDataHash =
/// SHA-256(challenge bytes)`. The UP flag bit (authenticatorData[32] & 0x01)
/// must be set — hardware-attested user presence is the point.
pub fn verify_fido2_es256(
    public_key_sec1: &[u8],
    signature_der: &[u8],
    challenge_bytes: &[u8],
    authenticator_data: &[u8],
) -> Result<Fido2Verified, &'static str> {
    use p256::ecdsa::signature::Verifier;
    // authenticatorData layout: rpIdHash(32) || flags(1) || signCount(4) || …
    if authenticator_data.len() < 37 {
        return Err("authenticatorData shorter than 37 bytes");
    }
    if authenticator_data[32] & 0x01 == 0 {
        return Err("UP (user-presence) flag not set in authenticatorData");
    }
    let counter = u32::from_be_bytes(authenticator_data[33..37].try_into().expect("4-byte slice"));
    let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key_sec1)
        .map_err(|_| "enrolled public key is not a valid SEC1 P-256 point")?;
    let sig = p256::ecdsa::Signature::from_der(signature_der)
        .map_err(|_| "signature is not DER (raw r||s is rejected — spec §5)")?;
    let client_data_hash = Sha256::digest(challenge_bytes);
    let mut message = Vec::with_capacity(authenticator_data.len() + 32);
    message.extend_from_slice(authenticator_data);
    message.extend_from_slice(&client_data_hash);
    key.verify(&message, &sig)
        .map_err(|_| "ES256 verification failed over authenticatorData || clientDataHash")?;
    Ok(Fido2Verified { counter })
}

/// Signed-material invariant — the persisted signature material an active
/// arm carries so the RESERVE can re-verify the ES256 signature at spend time,
/// not just trust attacker-writable columns. Mirrors exactly the inputs the
/// confirm-time `verify` closure consumed (api.rs): for `se-p256` only
/// `signature_der` + `challenge_bytes`; for `fido2-es256` also
/// `authenticator_data`, and — on the browser path — the `client_data_json`
/// whose embedded challenge must equal `challenge_bytes`.
#[derive(Debug, Clone)]
pub struct SignedArmMaterial<'a> {
    pub credential_id: &'a str,
    pub credential_type: &'a str,
    pub challenge_bytes: &'a [u8],
    pub signature_der: &'a [u8],
    pub authenticator_data: Option<&'a [u8]>,
    pub client_data_json: Option<&'a [u8]>,
}

/// The persisted `active_arm` singleton row (id = 0), read column-for-column.
/// Owned so it can cross closure boundaries; consumed by [`verify_arm_row`].
#[derive(Debug, Clone)]
pub struct ActiveArmRow {
    pub build_id: String,
    pub enabled_surface: String,
    pub effective_daily_cap_cents: i64,
    pub tenant: String,
    pub armed_epoch: i64,
    pub exp_at_ms: i64,
    pub challenge_bytes: Vec<u8>,
    pub signature_der: Vec<u8>,
    pub credential_id: String,
    pub credential_type: String,
    pub authenticator_data: Option<Vec<u8>>,
    pub client_data_json: Option<Vec<u8>>,
}

/// The ONE arm-validity decision, shared by claim-reserve (spend) and
/// staged-row recovery (sign). Extracted from `claim_reserve_impl` so the two
/// money gates can never drift (share-the-struct doctrine, not a mirror).
///
/// Verifies, in order, all fail-closed:
/// 0. Clock sanity: a saturated/pre-epoch `now_ms` (<= 0) refuses outright —
///    the freshness check is meaningless without a valid host clock.
/// 1. ES256 signature re-verify against the boot registry ([`reverify_signed_arm`]);
///    no registry → refuse.
/// 2. Signed-content assertion: the AUTHENTICATED challenge bytes must equal
///    the row columns AND the running binary (build_id, surface, cap, tenant).
///    A DB-write attacker can forge any column but not the P-256 signature.
/// 3. Freshness: the SIGNED spend-window deadline (`iat_ms + spend_window_ms`,
///    checked_add), plus the column consistency tripwire (`exp_at_ms` must
///    equal the computed deadline).
///
/// Returns the SIGNED `effective_daily_cap_cents` on success — the caller
/// applies any ambient ceiling (`.min`). The error string is a stable refusal
/// tag for audit/log lines, never attacker-controlled content.
pub fn verify_arm_row(
    row: &ActiveArmRow,
    registry: Option<&AttestKeyRegistry>,
    now_ms: i64,
) -> Result<i64, &'static str> {
    // (0) Clock sanity. Both callers saturate a pre-epoch SystemTime to 0;
    // a zero (or negative) now would make the freshness check below pass for
    // ANY signed iat — fail-open on a broken wall clock. A spend/sign gate
    // refuses instead: no valid host clock, no valid arm.
    if now_ms <= 0 {
        return Err("clock_invalid");
    }
    // (1) Re-verify the hardware signature — the single anchor.
    let Some(reg) = registry else {
        return Err("registry_unloaded");
    };
    let material = SignedArmMaterial {
        credential_id: &row.credential_id,
        credential_type: &row.credential_type,
        challenge_bytes: &row.challenge_bytes,
        signature_der: &row.signature_der,
        authenticator_data: row.authenticator_data.as_deref(),
        client_data_json: row.client_data_json.as_deref(),
    };
    if reverify_signed_arm(reg, &material).is_err() {
        return Err("bad_signature");
    }
    // (2) Signed content == columns == running binary.
    let signed: ArmConfirmChallenge = match serde_json::from_slice(&row.challenge_bytes) {
        Ok(c) => c,
        Err(_) => return Err("bad_challenge"),
    };
    if signed.v < 3
        || signed.build_id != row.build_id
        || signed.enabled_surface != row.enabled_surface
        || signed.effective_daily_cap_cents != row.effective_daily_cap_cents
        || signed.tenant != row.tenant
        || signed.build_id != build_id()
        || signed.enabled_surface != ENABLED_SURFACE_WATCH_PRODUCER
        || signed.tenant != CANARY_TENANT
    {
        return Err("signed_content_mismatch");
    }
    // (3) Freshness off the SIGNED tap time + SIGNED window (Attested-arm); the
    // raw column is attacker-writable and is only the tripwire below.
    let window_ms = if crate::watch::db::signed_spend_window_enabled() {
        signed.spend_window_ms
    } else {
        crate::watch::db::arm_window_ms_bootlocked()
    };
    let Some(spend_deadline_ms) = signed.iat_ms.checked_add(window_ms) else {
        return Err("window_overflow");
    };
    if now_ms >= spend_deadline_ms {
        return Err("window_expired");
    }
    if row.exp_at_ms != spend_deadline_ms {
        return Err("column_tripwire");
    }
    Ok(signed.effective_daily_cap_cents)
}

/// Signed-material invariant — re-verify a persisted arm's ES256 signature
/// against the BOOT registry, exactly as confirm did. This is the SINGLE verify
/// the reserve path uses; it reuses [`verify_se_p256`] / [`verify_fido2_es256`]
/// (and the browser clientDataJSON binding check) so confirm and reserve can
/// never drift. Fail-closed on registry-unloaded, unknown credential, or any
/// signature failure. A DB-write attacker who forges `active_arm` columns but
/// cannot produce a valid hardware signature is refused here.
pub fn reverify_signed_arm(
    registry: &AttestKeyRegistry,
    m: &SignedArmMaterial<'_>,
) -> Result<(), String> {
    use base64::Engine as _;
    if !registry.is_loaded() {
        return Err("registry_unloaded".to_string());
    }
    let Some(cred) = registry.get(m.credential_id) else {
        return Err("unknown_credential".to_string());
    };
    if cred.credential_type != m.credential_type {
        return Err("unknown_credential".to_string());
    }
    let pk = base64::engine::general_purpose::STANDARD
        .decode(&cred.public_key)
        .map_err(|_| "bad_signature".to_string())?;
    match cred.credential_type.as_str() {
        CREDENTIAL_TYPE_SE_P256 => {
            if m.authenticator_data.is_some() {
                return Err("bad_signature".to_string());
            }
            verify_se_p256(&pk, m.signature_der, m.challenge_bytes)
                .map_err(|_| "bad_signature".to_string())?;
        }
        CREDENTIAL_TYPE_FIDO2_ES256 => {
            let Some(ad) = m.authenticator_data else {
                return Err("bad_signature".to_string());
            };
            // Browser path: the signature is over clientDataJSON, whose embedded
            // challenge must equal the stored challenge (same replay check as
            // confirm). Native CTAP path verifies over the raw challenge bytes.
            let chal_for_fido2: &[u8] = if let Some(cdj) = m.client_data_json {
                let cdj_val: serde_json::Value =
                    serde_json::from_slice(cdj).map_err(|_| "bad_signature".to_string())?;
                let cdj_challenge_b64url = cdj_val
                    .get("challenge")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "bad_signature".to_string())?;
                let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(cdj_challenge_b64url)
                    .map_err(|_| "bad_signature".to_string())?;
                if decoded != m.challenge_bytes {
                    return Err("bad_signature".to_string());
                }
                if cdj_val.get("type").and_then(|v| v.as_str()) != Some("webauthn.get") {
                    return Err("bad_signature".to_string());
                }
                cdj
            } else {
                m.challenge_bytes
            };
            verify_fido2_es256(&pk, m.signature_der, chal_for_fido2, ad)
                .map_err(|_| "bad_signature".to_string())?;
        }
        _ => return Err("unknown_credential".to_string()),
    }
    Ok(())
}

/// Boot-time keyset hash, published once so the `boot_env_arm` audit path
/// (runner.rs) can chain it without threading the registry through every
/// signature (precedent: `process_instance_uuid`). "unloaded" when the
/// registry failed to load — the boot row records that state honestly.
static BOOT_KEYSET_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Publish the boot keyset hash (main.rs, once, right after registry load).
pub fn publish_boot_keyset_hash(registry: &AttestKeyRegistry) {
    let v = registry.keyset_hash().unwrap_or("unloaded").to_string();
    let _ = BOOT_KEYSET_HASH.set(v);
}

/// The published boot keyset hash ("unloaded" when the registry failed,
/// "unpublished" if boot never published — visible drift instead of a
/// silent empty string).
pub fn boot_keyset_hash() -> &'static str {
    BOOT_KEYSET_HASH
        .get()
        .map(|s| s.as_str())
        .unwrap_or("unpublished")
}

/// Signed-material invariant — the boot-loaded registry, published once so
/// the RESERVE (which runs on the SQLite thread with no handle to the arm
/// router's `Arc<AttestKeyRegistry>`) can re-verify a persisted arm's signature
/// without threading the registry through every claim caller (same precedent as
/// `BOOT_KEYSET_HASH` / `process_instance_uuid`). Unset until main.rs publishes;
/// the reserve treats an unpublished/unloaded registry as fail-closed.
static BOOT_REGISTRY: std::sync::OnceLock<std::sync::Arc<AttestKeyRegistry>> =
    std::sync::OnceLock::new();

/// Publish the boot registry (main.rs, once, right after load). Idempotent.
pub fn publish_boot_registry(registry: std::sync::Arc<AttestKeyRegistry>) {
    let _ = BOOT_REGISTRY.set(registry);
}

/// The published boot registry, if any. `None` until main.rs publishes — the
/// reserve maps that to a fail-closed refusal (no signature can be re-verified).
pub fn boot_registry() -> Option<std::sync::Arc<AttestKeyRegistry>> {
    BOOT_REGISTRY.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: build a v2 challenge with fixed content-binding fields so
    /// existing crypto tests only have to vary stage_id/timestamps.
    fn tv_challenge(
        stage_id: &str,
        staged_by: &str,
        iat_ms: i64,
        exp_ms: i64,
    ) -> anyhow::Result<Vec<u8>> {
        build_challenge_bytes(
            stage_id,
            staged_by,
            iat_ms,
            exp_ms,
            &ArmContent {
                build_id: "test-vector-build".to_string(),
                enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
                effective_daily_cap_cents: 5000,
                tenant: CANARY_TENANT.to_string(),
                effective_spend_window_ms: 86_400_000,
            },
        )
    }

    /// The exact canonical form is pinned — sorted keys, no whitespace,
    /// integer timestamps. If this assertion moves, `v` must bump.
    #[test]
    fn test_vector_canonical_bytes_are_pinned() {
        let bytes = sovereign_protocol::jcs::to_jcs_bytes(&pinned_test_vector()).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"build_id":"test-vector-build","deviation_tag":"dual-custody-local-attest","effective_daily_cap_cents":5000,"enabled_surface":"watch-producer","exp_ms":120000,"iat_ms":0,"kind":"arm-confirm-challenge","nonce":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","spend_window_ms":86400000,"stage_id":"00000000000000000000000000000000","staged_by":"test-vector","tenant":"canary","v":3}"#
        );
    }

    #[test]
    fn boot_self_test_passes_against_pinned_digest() {
        challenge_format_self_test().expect("self-test must pass against the pinned digest");
    }

    /// Float-saturation guard: a non-finite / negative /
    /// over-range ambient cap must FAIL CLOSED, never silently saturate to
    /// `i64::MAX` (INFINITY) or `0` (NaN) and bind a cap that was never real.
    #[test]
    fn derive_arm_content_rejects_unbindable_caps() {
        assert_eq!(
            derive_arm_content(f64::INFINITY, 86_400_000).unwrap_err(),
            "invalid_spend_cap"
        );
        assert_eq!(
            derive_arm_content(f64::NAN, 86_400_000).unwrap_err(),
            "invalid_spend_cap"
        );
        assert_eq!(
            derive_arm_content(-1.0, 86_400_000).unwrap_err(),
            "invalid_spend_cap"
        );
        // Attested-arm — a negative signed window is refused fail-closed.
        assert_eq!(
            derive_arm_content(50.0, -1).unwrap_err(),
            "invalid_spend_window"
        );
        // Regression guard: exercise the REAL 2^53-cents edge in BOTH
        // directions, in USD (the actual `derive_arm_content` input unit).
        // `MAX_ARM_CAP_CENTS_F64` is the inclusive ceiling on CENTS, so the
        // boundary lives at `MAX_ARM_CAP_CENTS_F64 / 100.0` USD.
        //
        // Just-OVER: the smallest cents value ABOVE the ceiling that survives the
        // USD round-trip must reject. At 2^53 cents the f64 ULP is 2.0 — `MAX+1.0`
        // is NOT representable as distinct from `MAX` here, and `(MAX+1.0)/100.0`
        // USD re-multiplies back to exactly `MAX` cents (binds), so it is the
        // wrong probe for "just over". `MAX+2.0` is the next representable cents
        // value; `(MAX+2.0)/100.0` USD round-trips to `MAX+2.0` cents, which falls
        // outside the inclusive `0..=MAX` range → `invalid_spend_cap`.
        assert_eq!(
            derive_arm_content((MAX_ARM_CAP_CENTS_F64 + 2.0) / 100.0, 86_400_000).unwrap_err(),
            "invalid_spend_cap"
        );
        // Just-UNDER: the ceiling itself (in USD) binds — `MAX` cents is the
        // largest f64-integer-exact value, so it round-trips and is IN range.
        let edge = derive_arm_content(MAX_ARM_CAP_CENTS_F64 / 100.0, 86_400_000)
            .expect("the inclusive 2^53-cents ceiling must bind");
        assert_eq!(edge.effective_daily_cap_cents, MAX_ARM_CAP_CENTS_F64 as i64);
        // A normal cap binds cleanly to integer cents.
        let ok = derive_arm_content(50.0, 86_400_000).expect("a finite in-range cap must bind");
        assert_eq!(ok.effective_daily_cap_cents, 5000);
        assert_eq!(ok.effective_spend_window_ms, 86_400_000);
    }

    /// B2 acceptance: a corrupted pin fails LOUDLY (Err → main.rs FATAL).
    #[test]
    fn boot_self_test_fails_on_corrupted_vector() {
        let err = challenge_format_self_test_against(
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect_err("corrupted pinned digest must fail the self-test");
        assert!(err.to_string().contains("format drift"));
    }

    /// Challenge bytes embed the binding fields and a fresh nonce each
    /// stage; two builds for the same stage differ ONLY by nonce (which is
    /// why canonicalization happens once and the stored bytes are the truth).
    #[test]
    fn build_challenge_embeds_binding_fields() {
        let bytes = tv_challenge("deadbeef", "sovereign-op", 1_000, 121_000).unwrap();
        let parsed: ArmConfirmChallenge = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.v, CHALLENGE_FORMAT_VERSION);
        assert_eq!(parsed.kind, CHALLENGE_KIND);
        assert_eq!(parsed.stage_id, "deadbeef");
        assert_eq!(parsed.staged_by, "sovereign-op");
        assert_eq!(parsed.iat_ms, 1_000);
        assert_eq!(parsed.exp_ms, 121_000);
        assert_eq!(parsed.deviation_tag, CHALLENGE_DEVIATION_TAG);
        use base64::Engine as _;
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(parsed.nonce)
            .unwrap();
        assert_eq!(nonce.len(), 32, "nonce must be 32 CSPRNG bytes");

        let again = tv_challenge("deadbeef", "sovereign-op", 1_000, 121_000).unwrap();
        assert_ne!(bytes, again, "every stage gets a fresh nonce");
    }

    // -----------------------------------------------------------------------
    // B3 — registry loader (spec §7.2)
    // -----------------------------------------------------------------------

    /// b64 of a 33-byte SEC1 compressed point (0x02 || 32 zero bytes).
    const PK_B64: &str = "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn vector_registry_json() -> String {
        // Deliberately UNSORTED so the sort-then-hash is exercised.
        format!(
            r#"[{{"credential_id":"ffff0000","credential_type":"fido2-es256","public_key":"{PK_B64}","label":"backup-usb","enrolled_at":"2026-06-12T00:00:00Z"}},{{"credential_id":"aaaa0000","credential_type":"se-p256","public_key":"{PK_B64}","label":"touch-id","enrolled_at":"2026-06-12T00:00:00Z"}}]"#
        )
    }

    /// B3 acceptance: the keyset hash matches an INDEPENDENT computation —
    /// pinned from `jq -cS 'sort_by(.credential_id)' <file> | tr -d '\n' |
    /// shasum -a 256` over the same vector.
    #[test]
    fn registry_keyset_hash_matches_independent_jq_shasum() {
        let reg = AttestKeyRegistry::parse(&vector_registry_json());
        assert!(reg.is_loaded());
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.keyset_hash().unwrap(),
            "a847e3b017f15cf0a45f3c64b175274167c83910e57df86f3f6d96f5fb8deed4"
        );
        // Sorted lookup works for both ids.
        assert!(reg.get("aaaa0000").is_some());
        assert_eq!(reg.get("ffff0000").unwrap().credential_type, "fido2-es256");
        assert!(reg.get("nope").is_none());
    }

    /// B3 acceptance: fail-closed on parse error and duplicate id — plus the
    /// rest of the §7.2 rules.
    #[test]
    fn registry_fails_closed_on_every_rule_violation() {
        // Malformed JSON.
        assert!(!AttestKeyRegistry::parse("{not json").is_loaded());
        // Wrong shape.
        assert!(!AttestKeyRegistry::parse(r#"{"credential_id":"x"}"#).is_loaded());
        // Empty array.
        assert!(!AttestKeyRegistry::parse("[]").is_loaded());
        // Duplicate credential_id.
        let dup = format!(
            r#"[{{"credential_id":"aaaa0000","credential_type":"se-p256","public_key":"{PK_B64}","label":"a","enrolled_at":"t"}},{{"credential_id":"aaaa0000","credential_type":"fido2-es256","public_key":"{PK_B64}","label":"b","enrolled_at":"t"}}]"#
        );
        assert!(!AttestKeyRegistry::parse(&dup).is_loaded());
        // Unknown credential_type (also covers the no-hardware rule today).
        let unknown = format!(
            r#"[{{"credential_id":"aaaa0000","credential_type":"software-es256","public_key":"{PK_B64}","label":"a","enrolled_at":"t"}}]"#
        );
        assert!(!AttestKeyRegistry::parse(&unknown).is_loaded());
        // public_key not base64.
        let badb64 = r#"[{"credential_id":"aaaa0000","credential_type":"se-p256","public_key":"%%%","label":"a","enrolled_at":"t"}]"#;
        assert!(!AttestKeyRegistry::parse(badb64).is_loaded());
        // public_key wrong length (16 bytes).
        let short = r#"[{"credential_id":"aaaa0000","credential_type":"se-p256","public_key":"AAAAAAAAAAAAAAAAAAAAAA==","label":"a","enrolled_at":"t"}]"#;
        assert!(!AttestKeyRegistry::parse(short).is_loaded());
        // Unloaded registry rejects lookups and has no hash.
        let u = AttestKeyRegistry::unloaded();
        assert!(u.get("aaaa0000").is_none());
        assert!(u.keyset_hash().is_none());
        assert!(u.is_empty());
    }

    /// Keyset hash is order-independent: a re-ordered file hashes identically
    /// (the records are sorted before hashing).
    #[test]
    fn registry_keyset_hash_is_order_independent() {
        let sorted = format!(
            r#"[{{"credential_id":"aaaa0000","credential_type":"se-p256","public_key":"{PK_B64}","label":"touch-id","enrolled_at":"2026-06-12T00:00:00Z"}},{{"credential_id":"ffff0000","credential_type":"fido2-es256","public_key":"{PK_B64}","label":"backup-usb","enrolled_at":"2026-06-12T00:00:00Z"}}]"#
        );
        let a = AttestKeyRegistry::parse(&vector_registry_json());
        let b = AttestKeyRegistry::parse(&sorted);
        assert_eq!(a.keyset_hash(), b.keyset_hash());
    }

    // -----------------------------------------------------------------------
    // B4 — ES256 verify (spec §4.2/§5)
    // -----------------------------------------------------------------------

    use p256::ecdsa::signature::Signer;

    /// Deterministic test keypair (NOT a real credential — fixed scalar).
    fn test_keypair() -> (p256::ecdsa::SigningKey, Vec<u8>) {
        let sk = p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
        let pk_sec1 = sk
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .to_vec();
        (sk, pk_sec1)
    }

    /// Cross-language vector anchor (spec §5): clientDataHash for the pinned
    /// challenge vector IS its pinned canonical digest — the Swift helper's
    /// self-test (B7) pins the same hex.
    #[test]
    fn fido2_client_data_hash_matches_pinned_vector_digest() {
        let bytes = sovereign_protocol::jcs::to_jcs_bytes(&pinned_test_vector()).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            "6d16bab2218abb0f9397dd294f0da6ddbd736289a788be2caa3d72b9d6cd4188"
        );
    }

    #[test]
    fn se_p256_der_signature_verifies_and_binds_to_challenge() {
        let (sk, pk) = test_keypair();
        let challenge = tv_challenge("cafe0001", "sovereign-op", 0, 120_000).unwrap();
        let sig: p256::ecdsa::Signature = sk.sign(&challenge);
        let der = sig.to_der();

        verify_se_p256(&pk, der.as_bytes(), &challenge).expect("valid DER sig must verify");

        // Wrong-txid: a signature over a DIFFERENT stage's challenge fails.
        let other = tv_challenge("dead0002", "sovereign-op", 0, 120_000).unwrap();
        assert!(verify_se_p256(&pk, der.as_bytes(), &other).is_err());

        // Raw r||s is rejected outright (DER only — spec §5).
        let raw_rs = sig.to_bytes();
        assert_eq!(raw_rs.len(), 64);
        assert!(
            verify_se_p256(&pk, &raw_rs, &challenge)
                .unwrap_err()
                .contains("DER"),
            "raw r||s must fail the DER parse, not the curve math"
        );

        // Unknown key: same sig against a different enrolled key fails.
        let other_pk_sec1 = p256::ecdsa::SigningKey::from_bytes(&[9u8; 32].into())
            .unwrap()
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .to_vec();
        assert!(verify_se_p256(&other_pk_sec1, der.as_bytes(), &challenge).is_err());
    }

    /// Build a minimal valid authenticatorData: rpIdHash(32) || flags(1) ||
    /// signCount(4 BE).
    fn auth_data(flags: u8, counter: u32) -> Vec<u8> {
        let mut ad = vec![0u8; 32];
        ad.push(flags);
        ad.extend_from_slice(&counter.to_be_bytes());
        ad
    }

    #[test]
    fn fido2_composition_up_flag_and_counter() {
        let (sk, pk) = test_keypair();
        let challenge = tv_challenge("cafe0003", "sovereign-op", 0, 120_000).unwrap();
        let ad = auth_data(0x01, 41);
        let mut message = ad.clone();
        message.extend_from_slice(&Sha256::digest(&challenge));
        let sig: p256::ecdsa::Signature = sk.sign(&message);
        let der = sig.to_der();

        let ok = verify_fido2_es256(&pk, der.as_bytes(), &challenge, &ad)
            .expect("valid CTAP composition must verify");
        assert_eq!(ok.counter, 41, "counter extracted from BE bytes 33..37");

        // UP flag missing → refused before any curve math.
        let ad_noup = auth_data(0x04, 42);
        let mut m2 = ad_noup.clone();
        m2.extend_from_slice(&Sha256::digest(&challenge));
        let sig2: p256::ecdsa::Signature = sk.sign(&m2);
        assert!(
            verify_fido2_es256(&pk, sig2.to_der().as_bytes(), &challenge, &ad_noup)
                .unwrap_err()
                .contains("UP")
        );

        // Truncated authenticatorData → refused.
        assert!(verify_fido2_es256(&pk, der.as_bytes(), &challenge, &[0u8; 36]).is_err());

        // Signature over a different challenge fails composition verify.
        let other = tv_challenge("beef0004", "sovereign-op", 0, 120_000).unwrap();
        assert!(verify_fido2_es256(&pk, der.as_bytes(), &other, &ad).is_err());
    }

    #[test]
    fn fido2_browser_assertion_live_vector() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;

        let pk_bytes = b64
            .decode("Am7FNq1A3NVEkYU1Ga4jaZlg/4Nvj8LPxBmHEzMs/0jC")
            .unwrap();
        let sig_der = b64.decode("MEQCIBlnPE2GhtdgeD9qRu2UW8eV86L4xEwVKc6YR33Ei3xhAiBsmumCGXWoVqeS8kwsfVbl0OUGZjxvhvGMTV3ZhI3MyA==").unwrap();
        let auth_data = b64
            .decode("ORVaBM2Yh6dFeo4DSt4Kv+Lnb09wynK5UD3iHvD1igoFAAAADg==")
            .unwrap();
        let cdj_bytes = b64.decode("eyJ0eXBlIjoid2ViYXV0aG4uZ2V0IiwiY2hhbGxlbmdlIjoiZXlKa1pYWnBZWFJwYjI1ZmRHRm5Jam9pWkhWaGJDMWpkWE4wYjJSNUxXeHZZMkZzTFdGMGRHVnpkQ0lzSW1WNGNGOXRjeUk2TVRjNE1UYzFPRGN4TnpNMk5Dd2lhV0YwWDIxeklqb3hOemd4TnpVNE5UazNNelkwTENKcmFXNWtJam9pWVhKdExXTnZibVpwY20wdFkyaGhiR3hsYm1kbElpd2libTl1WTJVaU9pSTBTVFZ5VDBaRFZrWlJhVzl5TTJGWGEzcExVV3N6UVdVMWVFa3pVa2QyTVdveWJsVmhOM2xYVURGblBTSXNJbk4wWVdkbFgybGtJam9pTURBeVlUTTROekl5TjJNME9UTmpNV1JoTUROalkyRmxNMk5sTmprME9EWWlMQ0p6ZEdGblpXUmZZbmtpT2lKemIzWmxjbVZwWjI0dGIzQWlMQ0oySWpveGZRd0FBT29BIiwib3JpZ2luIjoiaHR0cHM6Ly9nYXRld2F5LmxvY2FsOjg0NDMiLCJjcm9zc09yaWdpbiI6ZmFsc2V9").unwrap();

        let result = verify_fido2_es256(&pk_bytes, &sig_der, &cdj_bytes, &auth_data);
        assert!(
            result.is_ok(),
            "browser FIDO2 assertion must verify: {:?}",
            result.err()
        );
    }
}
