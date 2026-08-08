// ==========================================================================
// gateway-ledger — Standalone ledger verification CLI.
//
// Subcommands:
//   verify <db-path> --key <path> [--old-key <path>]
//       Verifies hash chain integrity and Ed25519 signatures against the
//       configured trust set (active + optional rotated old key, expanded by
//       verified key_introduce events).
//
//   fsck <db-path> --key <path> [--old-key <path>]
//       Full semantic check: chain + signatures + schema monotonicity +
//       signing_key_pubkey presence + key lifecycle event scanning.
//
//   Both commands refuse to run without --key unless --hash-only is set
//   (hash/link diagnostics only; signatures are NOT verified).
//
// Exits 0 on valid, 1 on tampered/unhealthy, 2 on usage/IO errors.
//
// Reads the SQLite DB read-only — safe to run against a live database.
// ==========================================================================

use ed25519_dalek::{Signature, VerifyingKey};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::process::ExitCode;

const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const EVENT_KEY_INTRODUCE: &str = "key_introduce";
const EVENT_KEY_REVOKE: &str = "key_revoke";

fn usage() -> ExitCode {
    eprintln!("Usage:");
    eprintln!(
        "  gateway-ledger verify <db-path> --key <signing-key-path> [--old-key <path>] [--hash-only]"
    );
    eprintln!(
        "  gateway-ledger fsck <db-path> --key <signing-key-path> [--old-key <path>] [--hash-only]"
    );
    eprintln!("  gateway-ledger generate-key <output-path>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  verify         Verify hash chain integrity and Ed25519 signatures");
    eprintln!("  fsck           Full semantic check (chain + signatures + key trust + schema)");
    eprintln!("  generate-key   Generate a 32-byte Ed25519 signing key seed file");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --key <path>       Active 32-byte Ed25519 signing key seed (required unless");
    eprintln!("                     --hash-only). Authoritative row-signing trust root.");
    eprintln!("  --old-key <path>   Previous configured key during rotation (optional).");
    eprintln!("                     Mirrors the sidecar old_verifying_key trust root.");
    eprintln!("  --hash-only        Hash/link check only; signatures are NOT verified.");
    eprintln!("                     Exit 0 means the chain is self-consistent, not signed.");
    ExitCode::from(2)
}

/// Parse `ROOT_PUBKEY_HEX` env var into a VerifyingKey. Returns `None` (with
/// a stderr note) when unset or malformed — keeps backward compatibility for
/// chains that predate the air-gapped-root model. When present, ceremony
/// (key_introduce / key_revoke) envelopes must be signed by this key, which
/// turns fsck into a real PKI trust check.
///
/// ROOT is ceremony-envelope authority only — never a ledger row-signing root.
fn load_root_pubkey_from_env() -> Option<VerifyingKey> {
    let hex_str = std::env::var("ROOT_PUBKEY_HEX").ok()?;
    let hex_str = hex_str.trim();
    if hex_str.is_empty() {
        return None;
    }
    if hex_str.len() != 64 {
        eprintln!(
            "⚠️  ROOT_PUBKEY_HEX must be 64 hex chars (got {}). Root verification disabled.",
            hex_str.len()
        );
        return None;
    }
    let bytes = match hex::decode(hex_str) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "⚠️  ROOT_PUBKEY_HEX is not valid hex ({}). Root verification disabled.",
                e
            );
            return None;
        }
    };
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    match VerifyingKey::from_bytes(&arr) {
        Ok(vk) => Some(vk),
        Err(e) => {
            eprintln!("⚠️  ROOT_PUBKEY_HEX is not a valid Ed25519 point ({}). Root verification disabled.", e);
            None
        }
    }
}

fn load_verifying_key(path: &str) -> Result<VerifyingKey, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Error reading key file {}: {}", path, e))?;
    let seed = if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        arr
    } else if bytes.len() == 64 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes[..32]);
        arr
    } else {
        return Err(format!(
            "Key file must be 32 or 64 bytes, got {}",
            bytes.len()
        ));
    };
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    Ok(signing_key.verifying_key())
}

/// Configured row-signing trust roots (mirrors sidecar active + old_verifying_key).
#[derive(Clone, Default)]
struct TrustKeys {
    active: Option<VerifyingKey>,
    old: Option<VerifyingKey>,
}

impl TrustKeys {
    fn has_row_key(&self) -> bool {
        self.active.is_some()
    }

    fn configured_pubkey_hexes(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        if let Some(ref vk) = self.active {
            set.insert(hex::encode(vk.as_bytes()));
        }
        if let Some(ref vk) = self.old {
            set.insert(hex::encode(vk.as_bytes()));
        }
        set
    }
}

struct EventRow {
    id: i64,
    timestamp: u64,
    source: String,
    target: String,
    payload: String,
    metadata: String,
    caller_key: Option<String>,
    signing_key_pubkey: Option<String>,
    schema_version: u32,
    prev_hash: String,
    hash: String,
    signature: String,
}

fn read_events(conn: &rusqlite::Connection) -> Result<Vec<EventRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, source, target, payload, metadata, caller_key, \
         signing_key_pubkey, schema_version, prev_hash, hash, signature \
         FROM audit_events ORDER BY id ASC",
        )
        .map_err(|e| format!("Error preparing query: {}", e))?;

    let rows: Vec<EventRow> = stmt
        .query_map([], |row| {
            Ok(EventRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                source: row.get(2)?,
                target: row.get(3)?,
                payload: row.get(4)?,
                metadata: row.get(5)?,
                caller_key: row.get::<_, Option<String>>(6)?,
                signing_key_pubkey: row.get::<_, Option<String>>(7)?,
                schema_version: row.get(8)?,
                prev_hash: row.get(9)?,
                hash: row.get(10)?,
                signature: row.get(11)?,
            })
        })
        .map_err(|e| format!("Error querying events: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Error reading event: {}", e))?;

    Ok(rows)
}

fn compute_hash(event: &EventRow) -> String {
    let data_to_hash = match event.schema_version {
        1 => format!(
            "{}|{}|{}|{}|{}|{}|{}",
            event.timestamp,
            event.source,
            event.target,
            event.payload,
            event.metadata,
            event.schema_version,
            event.prev_hash
        ),
        2 => {
            let ck = event.caller_key.clone().unwrap_or_default();
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}",
                event.timestamp,
                event.source,
                event.target,
                event.payload,
                event.metadata,
                event.schema_version,
                ck,
                event.prev_hash
            )
        }
        _ => {
            let ck = event.caller_key.clone().unwrap_or_default();
            let ts = event.timestamp.to_string();
            let sv = event.schema_version.to_string();
            format!(
                "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
                ts.len(),
                ts,
                event.source.len(),
                event.source,
                event.target.len(),
                event.target,
                event.payload.len(),
                event.payload,
                event.metadata.len(),
                event.metadata,
                sv.len(),
                sv,
                ck.len(),
                ck,
                event.prev_hash.len(),
                event.prev_hash,
            )
        }
    };
    let mut hasher = Sha256::new();
    hasher.update(data_to_hash.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify hash chain; when `hash_only` is false, also verify signatures against
/// the configured trust set (active + old + introduced). Row-stored
/// `signing_key_pubkey` is a selector within that set, never an implicit root.
fn verify_chain(
    events: &[EventRow],
    trust: &TrustKeys,
    hash_only: bool,
) -> Result<(u64, u64), (i64, String)> {
    let mut expected_prev = GENESIS_HASH.to_string();
    let mut verified: u64 = 0;
    let mut sig_verified: u64 = 0;

    let mut trusted_pubkeys = trust.configured_pubkey_hexes();
    let verify_sigs = !hash_only && trust.has_row_key();

    for event in events {
        if event.prev_hash != expected_prev {
            return Err((
                event.id,
                format!(
                    "chain broken: expected prev_hash={}, got={}",
                    expected_prev, event.prev_hash
                ),
            ));
        }

        let computed = compute_hash(event);
        if event.hash != computed {
            return Err((
                event.id,
                format!(
                    "hash mismatch: stored={}, computed={}",
                    event.hash, computed
                ),
            ));
        }

        if verify_sigs {
            let hash_bytes = hex::decode(&event.hash).unwrap();
            let sig_bytes = hex::decode(&event.signature)
                .map_err(|_| (event.id, "invalid signature hex".to_string()))?;
            let sig_array: [u8; 64] = sig_bytes
                .try_into()
                .map_err(|_| (event.id, "signature wrong length".to_string()))?;
            let sig = Signature::from_bytes(&sig_array);

            if let Some(ref pubkey_hex) = event.signing_key_pubkey {
                if !trusted_pubkeys.contains(pubkey_hex) {
                    return Err((
                        event.id,
                        format!(
                            "signing_key_pubkey not in trusted set (pubkey={})",
                            pubkey_hex
                        ),
                    ));
                }
                let pk_bytes = hex::decode(pubkey_hex)
                    .map_err(|_| (event.id, "invalid signing_key_pubkey hex".to_string()))?;
                let pk_arr: [u8; 32] = pk_bytes
                    .try_into()
                    .map_err(|_| (event.id, "signing_key_pubkey wrong length".to_string()))?;
                let event_vk = VerifyingKey::from_bytes(&pk_arr)
                    .map_err(|_| (event.id, "invalid public key".to_string()))?;
                if event_vk.verify_strict(&hash_bytes, &sig).is_err() {
                    return Err((
                        event.id,
                        format!("signature failed (signing_key_pubkey={})", pubkey_hex),
                    ));
                }
                sig_verified += 1;
            } else {
                // Legacy row without stored pubkey — trial active then old.
                let ok = trust
                    .active
                    .as_ref()
                    .map(|vk| vk.verify_strict(&hash_bytes, &sig).is_ok())
                    .unwrap_or(false)
                    || trust
                        .old
                        .as_ref()
                        .map(|vk| vk.verify_strict(&hash_bytes, &sig).is_ok())
                        .unwrap_or(false);
                if !ok {
                    return Err((event.id, "signature failed".to_string()));
                }
                sig_verified += 1;
            }

            // Expand trust after a verified key_introduce.
            if event.target == EVENT_KEY_INTRODUCE {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                    if let Some(new_pk) = payload.get("new_pubkey_hex").and_then(|v| v.as_str()) {
                        trusted_pubkeys.insert(new_pk.to_string());
                    }
                }
            }
        }

        expected_prev = event.hash.clone();
        verified += 1;
    }

    Ok((verified, sig_verified))
}

/// Fail closed when no row-signing key is configured and --hash-only was not
/// requested. Empty ledgers still need the gate when the operator expects a
/// signed verify (non-empty forges are the kill path; empty is harmless).
fn refuse_missing_key(trust: &TrustKeys, hash_only: bool) -> Option<ExitCode> {
    if hash_only || trust.has_row_key() {
        return None;
    }
    eprintln!(
        "❌ No ledger trust key supplied. Pass --key <path> (and optional --old-key), \
         or --hash-only for unsigned hash-chain diagnostics only."
    );
    Some(ExitCode::FAILURE)
}

fn cmd_verify(
    conn: &rusqlite::Connection,
    trust: &TrustKeys,
    root_pubkey: &Option<VerifyingKey>,
    hash_only: bool,
) -> ExitCode {
    if let Some(code) = refuse_missing_key(trust, hash_only) {
        return code;
    }

    let events = match read_events(conn) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::from(2);
        }
    };

    if events.is_empty() {
        println!("✅ Ledger is empty (0 events) — nothing to verify.");
        if hash_only {
            println!("ℹ️  --hash-only: signatures were NOT verified.");
        }
        return ExitCode::SUCCESS;
    }

    if hash_only {
        println!(
            "Verifying {} events (hash-only; signatures NOT verified)...",
            events.len()
        );
    } else {
        println!("Verifying {} events...", events.len());
    }

    let mut healthy = true;
    match verify_chain(&events, trust, hash_only) {
        Ok((verified, sig_verified)) => {
            println!(
                "✅ Hash chain verified: {} events, all links valid.",
                verified
            );
            if hash_only {
                println!(
                    "ℹ️  --hash-only: signatures were NOT verified ({} events unchecked).",
                    verified
                );
            } else if sig_verified > 0 {
                println!(
                    "✅ Signatures verified: {}/{} events.",
                    sig_verified, verified
                );
            }
        }
        Err((id, msg)) => {
            eprintln!("❌ Event #{}: {}", id, msg);
            healthy = false;
        }
    }

    // Root pubkey ceremony cross-check (when ROOT_PUBKEY_HEX is configured).
    // Ceremony envelopes only — not a row-signing trust root.
    if !hash_only {
        if let Some(root_vk) = root_pubkey {
            let root_hex = hex::encode(root_vk.as_bytes());
            let mut violations = 0usize;
            let mut ceremonies = 0usize;
            for event in &events {
                if event.target != EVENT_KEY_INTRODUCE && event.target != EVENT_KEY_REVOKE {
                    continue;
                }
                ceremonies += 1;
                let parsed: serde_json::Value = match serde_json::from_str(&event.payload) {
                    Ok(v) => v,
                    Err(_) => {
                        violations += 1;
                        continue;
                    }
                };
                let signer_field = if event.target == EVENT_KEY_INTRODUCE {
                    "introduced_by_pubkey_hex"
                } else {
                    "revoked_by_pubkey_hex"
                };
                let env_sig = parsed
                    .get("envelope_signature_hex")
                    .and_then(|v| v.as_str());
                let signer_hex = parsed.get(signer_field).and_then(|v| v.as_str());
                match (env_sig, signer_hex) {
                    (Some(sig), Some(s)) => {
                        if s != root_hex {
                            eprintln!(
                                "❌ Event #{} ({}): signer != ROOT_PUBKEY_HEX",
                                event.id, event.target
                            );
                            violations += 1;
                        } else if !verify_ceremony_envelope(&event.target, &event.payload, sig, s)
                        {
                            eprintln!(
                                "❌ Event #{} ({}): root envelope signature invalid",
                                event.id, event.target
                            );
                            violations += 1;
                        }
                    }
                    _ => {
                        eprintln!(
                            "❌ Event #{} ({}): missing envelope_signature_hex or signer field",
                            event.id, event.target
                        );
                        violations += 1;
                    }
                }
            }
            if ceremonies == 0 {
                // no ceremonies recorded yet — nothing to check
            } else if violations == 0 {
                println!(
                    "✅ Root ceremony verification: {}/{} events signed by ROOT_PUBKEY_HEX.",
                    ceremonies, ceremonies
                );
            } else {
                healthy = false;
            }
        }
    }

    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Ordered introduce-before-use / revoke scan used by fsck (and tests).
/// Row trust roots = configured_signers only (never ROOT_PUBKEY_HEX).
struct KeyTrustScan {
    signers_seen: HashSet<String>,
    introduces: Vec<(i64, String)>,
    revokes: Vec<(i64, String)>,
    introduced_keys: HashSet<String>,
    revoked_keys: HashSet<String>,
    duplicate_introduces: Vec<String>,
    revoked_key_uses: Vec<i64>,
    unintroduced_signer_events: Vec<(i64, String)>,
    envelope_failures: Vec<(i64, String)>,
}

fn scan_key_trust(events: &[EventRow], configured_signers: &HashSet<String>) -> KeyTrustScan {
    let mut known_keys: HashSet<String> = configured_signers.clone();
    let mut signers_seen: HashSet<String> = HashSet::new();
    let mut introduces: Vec<(i64, String)> = Vec::new();
    let mut revokes: Vec<(i64, String)> = Vec::new();
    let mut introduced_keys: HashSet<String> = HashSet::new();
    let mut revoked_keys: HashSet<String> = HashSet::new();
    let mut duplicate_introduces: Vec<String> = Vec::new();
    let mut revoked_key_uses: Vec<i64> = Vec::new();
    let mut unintroduced_signer_events: Vec<(i64, String)> = Vec::new();
    let mut envelope_failures: Vec<(i64, String)> = Vec::new();

    for event in events {
        if let Some(ref pk) = event.signing_key_pubkey {
            signers_seen.insert(pk.clone());
            if !known_keys.contains(pk) {
                unintroduced_signer_events.push((event.id, pk.clone()));
            }
            if revoked_keys.contains(pk) {
                revoked_key_uses.push(event.id);
            }
        }

        if event.target == EVENT_KEY_INTRODUCE {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                if let Some(new_pk) = payload.get("new_pubkey_hex").and_then(|v| v.as_str()) {
                    if introduced_keys.contains(new_pk) {
                        duplicate_introduces.push(new_pk.to_string());
                    }
                    introduced_keys.insert(new_pk.to_string());
                    known_keys.insert(new_pk.to_string());
                    introduces.push((event.id, new_pk.to_string()));
                }
                if let (Some(env_sig), Some(signer_hex)) = (
                    payload
                        .get("envelope_signature_hex")
                        .and_then(|v| v.as_str()),
                    payload
                        .get("introduced_by_pubkey_hex")
                        .and_then(|v| v.as_str()),
                ) {
                    if event.signing_key_pubkey.as_deref() != Some(signer_hex) {
                        envelope_failures.push((
                            event.id,
                            "introduce envelope signer mismatches chain signing_key_pubkey".into(),
                        ));
                    } else if !verify_ceremony_envelope(
                        EVENT_KEY_INTRODUCE,
                        &event.payload,
                        env_sig,
                        signer_hex,
                    ) {
                        envelope_failures
                            .push((event.id, "introduce envelope signature invalid".into()));
                    }
                }
            }
        } else if event.target == EVENT_KEY_REVOKE {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                if let Some(revoked_pk) = payload.get("revoked_pubkey_hex").and_then(|v| v.as_str())
                {
                    revoked_keys.insert(revoked_pk.to_string());
                    revokes.push((event.id, revoked_pk.to_string()));
                }
                if let (Some(env_sig), Some(signer_hex)) = (
                    payload
                        .get("envelope_signature_hex")
                        .and_then(|v| v.as_str()),
                    payload
                        .get("revoked_by_pubkey_hex")
                        .and_then(|v| v.as_str()),
                ) {
                    if event.signing_key_pubkey.as_deref() != Some(signer_hex) {
                        envelope_failures.push((
                            event.id,
                            "revoke envelope signer mismatches chain signing_key_pubkey".into(),
                        ));
                    } else if !verify_ceremony_envelope(
                        EVENT_KEY_REVOKE,
                        &event.payload,
                        env_sig,
                        signer_hex,
                    ) {
                        envelope_failures
                            .push((event.id, "revoke envelope signature invalid".into()));
                    }
                }
            }
        }
    }

    KeyTrustScan {
        signers_seen,
        introduces,
        revokes,
        introduced_keys,
        revoked_keys,
        duplicate_introduces,
        revoked_key_uses,
        unintroduced_signer_events,
        envelope_failures,
    }
}

fn cmd_fsck(
    conn: &rusqlite::Connection,
    trust: &TrustKeys,
    root_pubkey: &Option<VerifyingKey>,
    hash_only: bool,
) -> ExitCode {
    if let Some(code) = refuse_missing_key(trust, hash_only) {
        return code;
    }

    let events = match read_events(conn) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::from(2);
        }
    };

    if events.is_empty() {
        println!("✅ Ledger is empty (0 events) — nothing to check.");
        if hash_only {
            println!("ℹ️  --hash-only: signatures were NOT verified.");
        }
        println!("✅ fsck complete — chain is HEALTHY");
        return ExitCode::SUCCESS;
    }

    if hash_only {
        println!(
            "Running ledger fsck ({} events, hash-only; signatures NOT verified)...\n",
            events.len()
        );
    } else {
        println!("Running full ledger fsck ({} events)...\n", events.len());
    }
    let mut healthy = true;

    // --- 1. Hash chain + signatures ---
    match verify_chain(&events, trust, hash_only) {
        Ok((verified, sig_verified)) => {
            println!("✅ Hash chain valid ({} events)", verified);
            if hash_only {
                println!(
                    "ℹ️  --hash-only: signatures were NOT verified ({} events unchecked)",
                    verified
                );
            } else if sig_verified > 0 {
                println!("✅ Signatures valid ({}/{})", sig_verified, verified);
            }
        }
        Err((id, msg)) => {
            eprintln!("❌ Hash/signature failure at event #{}: {}", id, msg);
            healthy = false;
        }
    }

    // --- 2. Schema version monotonicity ---
    let mut max_sv: u32 = 0;
    let mut sv_violations: Vec<(i64, u32, u32)> = Vec::new();
    for event in &events {
        if event.schema_version < max_sv {
            sv_violations.push((event.id, event.schema_version, max_sv));
        }
        max_sv = max_sv.max(event.schema_version);
    }
    if sv_violations.is_empty() {
        println!("✅ Schema versions monotonic (max=v{})", max_sv);
    } else {
        eprintln!("❌ Schema version regression:");
        for (id, got, expected_min) in &sv_violations {
            eprintln!("   event #{}: v{} < v{}", id, got, expected_min);
        }
        healthy = false;
    }

    // --- 3. signing_key_pubkey presence on v3+ ---
    let missing_pubkey: Vec<i64> = events
        .iter()
        .filter(|e| e.schema_version >= 3 && e.signing_key_pubkey.is_none())
        .map(|e| e.id)
        .collect();
    if missing_pubkey.is_empty() {
        println!("✅ All v3+ events carry signing_key_pubkey");
    } else {
        let preview: Vec<String> = missing_pubkey
            .iter()
            .take(5)
            .map(|id| format!("#{}", id))
            .collect();
        println!(
            "⚠️  {} v3+ event(s) missing signing_key_pubkey: {}{}",
            missing_pubkey.len(),
            preview.join(", "),
            if missing_pubkey.len() > 5 { "..." } else { "" }
        );
    }

    // --- 4. Key trust chain (row signers = --key + --old-key only; not ROOT) ---
    let configured_signers = trust.configured_pubkey_hexes();
    let scan = scan_key_trust(&events, &configured_signers);

    println!("✅ Key trust chain:");
    for pk in &scan.signers_seen {
        let short = &pk[..pk.len().min(12)];
        let status = if scan.revoked_keys.contains(pk) {
            "REVOKED"
        } else {
            "active"
        };
        let intro = scan.introduces.iter().find(|(_, p)| p == pk);
        let intro_str = if configured_signers.contains(pk) {
            "configured root".to_string()
        } else if let Some((eid, _)) = intro {
            format!("introduced at event #{}", eid)
        } else {
            "UNKNOWN (not configured, not introduced)".to_string()
        };
        println!("   - {}... ({}, {})", short, status, intro_str);
    }
    println!(
        "   Key lifecycle: {} introduce(s), {} revoke(s)",
        scan.introduces.len(),
        scan.revokes.len()
    );

    // Introduce-before-use: signers_seen ⊆ introduced ∪ configured.
    // Skip policy walk under --hash-only (no cryptographic pin).
    if hash_only {
        println!("ℹ️  --hash-only: signer trust policy skipped (signatures NOT verified)");
    } else if configured_signers.is_empty() {
        // Should be unreachable after refuse_missing_key unless empty key set.
        eprintln!("❌ No configured row-signing keys for trust policy");
        healthy = false;
    } else if scan.unintroduced_signer_events.is_empty() {
        println!("✅ All signers configured or introduced before use");
    } else {
        for (id, pk) in &scan.unintroduced_signer_events {
            let short = &pk[..pk.len().min(12)];
            eprintln!(
                "❌ Event #{}: signing_key_pubkey {}... not in trusted set",
                id, short
            );
        }
        healthy = false;
    }

    // --- 5. Revoked key usage ---
    if scan.revoked_key_uses.is_empty() {
        println!("✅ No revoked-key usage detected");
    } else {
        eprintln!(
            "❌ {} event(s) signed by revoked keys: {:?}",
            scan.revoked_key_uses.len(),
            scan.revoked_key_uses
        );
        healthy = false;
    }

    // --- 6. Duplicate introduces ---
    if !scan.duplicate_introduces.is_empty() {
        eprintln!(
            "❌ Duplicate introduce events for: {:?}",
            scan.duplicate_introduces
        );
        healthy = false;
    }

    // --- 7. Envelope signature validity ---
    if scan.envelope_failures.is_empty() {
        if !scan.introduces.is_empty() || !scan.revokes.is_empty() {
            println!("✅ Ceremony envelope signatures valid");
        }
    } else {
        for (id, msg) in &scan.envelope_failures {
            eprintln!("❌ Event #{}: {}", id, msg);
        }
        healthy = false;
    }

    // --- 8. Root pubkey cross-check (ceremony envelopes only) ---
    if hash_only {
        // skip root ceremony under hash-only
    } else if let Some(root_vk) = root_pubkey {
        let root_hex = hex::encode(root_vk.as_bytes());
        let mut root_violations: Vec<(i64, String, String)> = Vec::new();
        for event in &events {
            if event.target != EVENT_KEY_INTRODUCE && event.target != EVENT_KEY_REVOKE {
                continue;
            }
            let parsed: serde_json::Value = match serde_json::from_str(&event.payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let signer_field = if event.target == EVENT_KEY_INTRODUCE {
                "introduced_by_pubkey_hex"
            } else {
                "revoked_by_pubkey_hex"
            };
            let signer_hex = match parsed.get(signer_field).and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            if signer_hex != root_hex {
                root_violations.push((event.id, event.target.clone(), signer_hex.to_string()));
            }
        }
        if root_violations.is_empty() {
            println!("✅ All ceremony events signed by ROOT_PUBKEY_HEX");
        } else {
            for (id, target, signer) in &root_violations {
                let short = &signer[..signer.len().min(12)];
                eprintln!(
                    "❌ Event #{} ({}): not signed by root (signer={}...)",
                    id, target, short
                );
            }
            healthy = false;
        }
    } else if !scan.introduces.is_empty() || !scan.revokes.is_empty() {
        println!("ℹ️  Root cross-check skipped (ROOT_PUBKEY_HEX not set)");
    }

    // --- Final verdict ---
    println!();
    if healthy {
        if hash_only {
            println!("✅ fsck complete — hash chain self-consistent (signatures NOT verified)");
        } else {
            println!("✅ fsck complete — chain is HEALTHY");
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("❌ fsck complete — chain is UNHEALTHY");
        ExitCode::FAILURE
    }
}

fn verify_ceremony_envelope(
    event_type: &str,
    payload_json: &str,
    envelope_sig_hex: &str,
    signer_pubkey_hex: &str,
) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(payload_json) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let preimage = if event_type == EVENT_KEY_INTRODUCE {
        let new_pk = match parsed.get("new_pubkey_hex").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return false,
        };
        let purpose = match parsed.get("purpose").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return false,
        };
        let tag = b"GW-INTRODUCE-v1\0";
        let body = format!(
            "{}:{}|{}:{}|{}:{}",
            new_pk.len(),
            new_pk,
            purpose.len(),
            purpose,
            signer_pubkey_hex.len(),
            signer_pubkey_hex,
        );
        [tag.as_slice(), body.as_bytes()].concat()
    } else if event_type == EVENT_KEY_REVOKE {
        let revoked_pk = match parsed.get("revoked_pubkey_hex").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return false,
        };
        let reason = match parsed.get("reason").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return false,
        };
        let tag = b"GW-REVOKE-v1\0";
        let body = format!(
            "{}:{}|{}:{}|{}:{}",
            revoked_pk.len(),
            revoked_pk,
            reason.len(),
            reason,
            signer_pubkey_hex.len(),
            signer_pubkey_hex,
        );
        [tag.as_slice(), body.as_bytes()].concat()
    } else {
        return false;
    };

    let mut hasher = Sha256::new();
    hasher.update(&preimage);
    let digest = hasher.finalize();

    let sig_bytes = match hex::decode(envelope_sig_hex) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let sig_array: [u8; 64] = sig_bytes.try_into().unwrap();
    let sig = Signature::from_bytes(&sig_array);

    let pk_bytes = match hex::decode(signer_pubkey_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => return false,
    };

    match VerifyingKey::from_bytes(&pk_bytes) {
        Ok(vk) => vk.verify_strict(&digest, &sig).is_ok(),
        Err(_) => false,
    }
}

fn cmd_generate_key(output_path: &str) -> ExitCode {
    use std::os::unix::fs::PermissionsExt;

    if std::path::Path::new(output_path).exists() {
        eprintln!(
            "Error: {} already exists — refusing to overwrite",
            output_path
        );
        return ExitCode::from(2);
    }

    let mut key_bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut key_bytes);

    if let Err(e) = std::fs::write(output_path, key_bytes) {
        eprintln!("Error writing key file: {}", e);
        return ExitCode::from(2);
    }

    if let Err(e) = std::fs::set_permissions(output_path, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("Warning: failed to set permissions to 0600: {}", e);
    }

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_bytes);
    let pubkey_hex = hex::encode(signing_key.verifying_key().as_bytes());

    println!("✅ Generated 32-byte Ed25519 signing key seed");
    println!("   Path:   {}", output_path);
    println!("   Perms:  0600");
    println!("   Pubkey: {}", pubkey_hex);

    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        return usage();
    }

    let command = &args[1];

    // generate-key doesn't need a DB connection
    if command == "generate-key" {
        return cmd_generate_key(&args[2]);
    }

    let db_path = &args[2];

    let mut key_path: Option<&str> = None;
    let mut old_key_path: Option<&str> = None;
    let mut hash_only = false;
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--key" && i + 1 < args.len() {
            key_path = Some(&args[i + 1]);
            i += 2;
        } else if args[i] == "--old-key" && i + 1 < args.len() {
            old_key_path = Some(&args[i + 1]);
            i += 2;
        } else if args[i] == "--hash-only" {
            hash_only = true;
            i += 1;
        } else {
            eprintln!("Unknown argument: {}", args[i]);
            return ExitCode::from(2);
        }
    }

    if old_key_path.is_some() && key_path.is_none() {
        eprintln!("Error: --old-key requires --key");
        return ExitCode::from(2);
    }

    let active = match key_path {
        Some(kp) => match load_verifying_key(kp) {
            Ok(vk) => Some(vk),
            Err(e) => {
                eprintln!("{}", e);
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let old = match old_key_path {
        Some(kp) => match load_verifying_key(kp) {
            Ok(vk) => Some(vk),
            Err(e) => {
                eprintln!("{}", e);
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let trust = TrustKeys { active, old };

    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error opening database {}: {}", db_path, e);
            return ExitCode::from(2);
        }
    };

    let root_pubkey = load_root_pubkey_from_env();

    match command.as_str() {
        "verify" => cmd_verify(&conn, &trust, &root_pubkey, hash_only),
        "fsck" => cmd_fsck(&conn, &trust, &root_pubkey, hash_only),
        _ => {
            eprintln!("Unknown command: {}", command);
            usage()
        }
    }
}

// ==========================================================================
// CLI verifier regression tests (findings 1–4)
// ==========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = hex::encode(sk.verifying_key().as_bytes());
        (sk, pk)
    }

    fn sign_row(sk: &SigningKey, mut row: EventRow) -> EventRow {
        row.hash = compute_hash(&row);
        let hash_bytes = hex::decode(&row.hash).unwrap();
        row.signature = hex::encode(sk.sign(&hash_bytes).to_bytes());
        row
    }

    fn base_row(id: i64, prev: &str, target: &str, payload: &str, pk: Option<&str>) -> EventRow {
        EventRow {
            id,
            timestamp: 1_700_000_000_000 + id as u64,
            source: "test".into(),
            target: target.into(),
            payload: payload.into(),
            metadata: "{}".into(),
            caller_key: None,
            signing_key_pubkey: pk.map(|s| s.to_string()),
            schema_version: 3,
            prev_hash: prev.to_string(),
            hash: String::new(),
            signature: String::new(),
        }
    }

    fn chain_two(sk_a: &SigningKey, pk_a: &str, sk_b: &SigningKey, pk_b: &str) -> Vec<EventRow> {
        let r1 = sign_row(
            sk_a,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"a"}"#, Some(pk_a)),
        );
        let r2 = sign_row(
            sk_b,
            base_row(2, &r1.hash, "t", r#"{"action":"b"}"#, Some(pk_b)),
        );
        vec![r1, r2]
    }

    #[test]
    fn refuse_missing_key_hard_fails_without_hash_only() {
        let empty = TrustKeys::default();
        assert!(refuse_missing_key(&empty, false).is_some());
        assert!(refuse_missing_key(&empty, true).is_none());

        let (sk, _) = keypair();
        let with_key = TrustKeys {
            active: Some(sk.verifying_key()),
            old: None,
        };
        assert!(refuse_missing_key(&with_key, false).is_none());
    }

    #[test]
    fn verify_rejects_attacker_key_and_pubkey() {
        let (legit, pk_legit) = keypair();
        let (attacker, pk_att) = keypair();
        let mut row = sign_row(
            &legit,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"ok"}"#, Some(&pk_legit)),
        );
        // Forge: rewrite payload, re-sign with attacker, store attacker pubkey.
        row.payload = r#"{"action":"stolen"}"#.into();
        row = sign_row(&attacker, row);
        row.signing_key_pubkey = Some(pk_att);

        let trust = TrustKeys {
            active: Some(legit.verifying_key()),
            old: None,
        };
        let err = verify_chain(&[row], &trust, false).unwrap_err();
        assert!(
            err.1.contains("not in trusted set") || err.1.contains("signature failed"),
            "unexpected err: {}",
            err.1
        );
    }

    #[test]
    fn verify_legacy_null_pubkey_under_configured_keys() {
        let (sk, _) = keypair();
        let row = sign_row(
            &sk,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"legacy"}"#, None),
        );
        let trust = TrustKeys {
            active: Some(sk.verifying_key()),
            old: None,
        };
        let (n, sigs) = verify_chain(&[row], &trust, false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sigs, 1);
    }

    #[test]
    fn verify_legacy_null_pubkey_accepts_old_key() {
        let (old_sk, _) = keypair();
        let (active_sk, _) = keypair();
        let row = sign_row(
            &old_sk,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"legacy-old"}"#, None),
        );
        let trust = TrustKeys {
            active: Some(active_sk.verifying_key()),
            old: Some(old_sk.verifying_key()),
        };
        assert!(verify_chain(&[row], &trust, false).is_ok());
    }

    #[test]
    fn verify_rotated_ledger_requires_active_and_old() {
        let (sk_a, pk_a) = keypair();
        let (sk_b, pk_b) = keypair();
        let events = chain_two(&sk_a, &pk_a, &sk_b, &pk_b);

        // Active B only — early A rows fail (kill-criterion scenario).
        let only_b = TrustKeys {
            active: Some(sk_b.verifying_key()),
            old: None,
        };
        assert!(verify_chain(&events, &only_b, false).is_err());

        // Active A only — later B rows fail.
        let only_a = TrustKeys {
            active: Some(sk_a.verifying_key()),
            old: None,
        };
        assert!(verify_chain(&events, &only_a, false).is_err());

        // Active B + old A — full rotated history verifies.
        let both = TrustKeys {
            active: Some(sk_b.verifying_key()),
            old: Some(sk_a.verifying_key()),
        };
        let (n, sigs) = verify_chain(&events, &both, false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(sigs, 2);
    }

    #[test]
    fn verify_introduced_key_after_introduce() {
        let (sk_a, pk_a) = keypair();
        let (sk_c, pk_c) = keypair();

        let r1 = sign_row(
            &sk_a,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"a"}"#, Some(&pk_a)),
        );
        let intro_payload = format!(
            r#"{{"new_pubkey_hex":"{}","purpose":"ledger_signing"}}"#,
            pk_c
        );
        let r2 = sign_row(
            &sk_a,
            base_row(2, &r1.hash, EVENT_KEY_INTRODUCE, &intro_payload, Some(&pk_a)),
        );
        let r3 = sign_row(
            &sk_c,
            base_row(3, &r2.hash, "t", r#"{"action":"c"}"#, Some(&pk_c)),
        );

        let trust = TrustKeys {
            active: Some(sk_a.verifying_key()),
            old: None,
        };
        let (n, sigs) = verify_chain(&[r1, r2, r3], &trust, false).unwrap();
        assert_eq!(n, 3);
        assert_eq!(sigs, 3);
    }

    #[test]
    fn verify_use_before_introduce_fails() {
        let (sk_a, _pk_a) = keypair();
        let (sk_c, pk_c) = keypair();
        let row = sign_row(
            &sk_c,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"early"}"#, Some(&pk_c)),
        );
        let trust = TrustKeys {
            active: Some(sk_a.verifying_key()),
            old: None,
        };
        let err = verify_chain(&[row], &trust, false).unwrap_err();
        assert!(err.1.contains("not in trusted set"), "{}", err.1);
    }

    #[test]
    fn hash_only_accepts_unsigned_consistent_chain() {
        let (sk, pk) = keypair();
        let mut row = sign_row(
            &sk,
            base_row(1, GENESIS_HASH, "t", r#"{"action":"x"}"#, Some(&pk)),
        );
        // Break the signature but keep a consistent hash chain.
        row.signature = "00".repeat(64);

        let empty = TrustKeys::default();
        let (n, sigs) = verify_chain(&[row], &empty, true).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sigs, 0, "hash-only must not count signature verifications");
    }

    #[test]
    fn hash_only_still_rejects_broken_hash_chain() {
        let row = EventRow {
            id: 1,
            timestamp: 1,
            source: "t".into(),
            target: "t".into(),
            payload: "{}".into(),
            metadata: "{}".into(),
            caller_key: None,
            signing_key_pubkey: None,
            schema_version: 3,
            prev_hash: GENESIS_HASH.into(),
            hash: "deadbeef".into(),
            signature: "00".repeat(64),
        };
        let empty = TrustKeys::default();
        let err = verify_chain(&[row], &empty, true).unwrap_err();
        assert!(err.1.contains("hash mismatch"), "{}", err.1);
    }

    #[test]
    fn scan_flags_unintroduced_signer() {
        let configured = HashSet::from(["aa".repeat(32)]);
        let attacker = "bb".repeat(32);
        let events = vec![base_row(
            1,
            GENESIS_HASH,
            "t",
            "{}",
            Some(&attacker),
        )];
        // Don't need real sigs for scan_key_trust.
        let scan = scan_key_trust(&events, &configured);
        assert_eq!(scan.unintroduced_signer_events.len(), 1);
        assert!(!scan.signers_seen.is_subset(
            &configured
                .union(&scan.introduced_keys)
                .cloned()
                .collect()
        ));
    }

    #[test]
    fn scan_root_pubkey_not_a_configured_row_signer() {
        // ROOT hex must not seed row trust; only --key / --old-key do.
        let root_hex = "cc".repeat(32);
        let attacker = root_hex.clone();
        let configured = HashSet::new(); // no --key
        let events = vec![base_row(1, GENESIS_HASH, "t", "{}", Some(&attacker))];
        let scan = scan_key_trust(&events, &configured);
        assert_eq!(scan.unintroduced_signer_events.len(), 1);
        assert!(!configured.contains(&root_hex));
    }

    #[test]
    fn scan_revoked_key_use_fails() {
        let (sk_a, pk_a) = keypair();
        let (sk_b, pk_b) = keypair();

        let r1 = base_row(1, GENESIS_HASH, "t", r#"{"a":1}"#, Some(&pk_a));
        let revoke_payload = format!(r#"{{"revoked_pubkey_hex":"{}"}}"#, pk_b);
        let r2 = base_row(
            2,
            "prev-placeholder",
            EVENT_KEY_REVOKE,
            &revoke_payload,
            Some(&pk_a),
        );
        let r3 = base_row(3, "prev-placeholder", "t", r#"{"a":3}"#, Some(&pk_b));

        let configured = HashSet::from([pk_a.clone(), pk_b.clone()]);
        let scan = scan_key_trust(&[r1, r2, r3], &configured);
        assert!(
            scan.revoked_key_uses.contains(&3),
            "event #3 signed by revoked key must be flagged: {:?}",
            scan.revoked_key_uses
        );
        let _ = (sk_a, sk_b); // keys used only for pubkey material
    }

    #[test]
    fn configured_signers_include_old_not_root() {
        let (a, _) = keypair();
        let (b, _) = keypair();
        let trust = TrustKeys {
            active: Some(a.verifying_key()),
            old: Some(b.verifying_key()),
        };
        let set = trust.configured_pubkey_hexes();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&hex::encode(a.verifying_key().as_bytes())));
        assert!(set.contains(&hex::encode(b.verifying_key().as_bytes())));
    }
}
