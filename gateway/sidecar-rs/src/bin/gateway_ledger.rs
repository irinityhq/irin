// ==========================================================================
// gateway-ledger — Standalone ledger verification CLI.
//
// Subcommands:
//   verify <db-path> [--key <signing-key-path>]
//       Verifies hash chain integrity and Ed25519 signatures.
//
//   fsck <db-path> [--key <signing-key-path>]
//       Full semantic check: chain + signatures + schema monotonicity +
//       signing_key_pubkey presence + key lifecycle event scanning.
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
    eprintln!("  gateway-ledger verify <db-path> [--key <signing-key-path>]");
    eprintln!("  gateway-ledger fsck <db-path> [--key <signing-key-path>]");
    eprintln!("  gateway-ledger generate-key <output-path>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  verify         Verify hash chain integrity and Ed25519 signatures");
    eprintln!("  fsck           Full semantic check (chain + signatures + key trust + schema)");
    eprintln!("  generate-key   Generate a 32-byte Ed25519 signing key seed file");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --key <path>   Path to the 32-byte Ed25519 signing key (raw bytes).");
    eprintln!("                 If omitted, only hash chain integrity is verified.");
    ExitCode::from(2)
}

/// Parse `ROOT_PUBKEY_HEX` env var into a VerifyingKey. Returns `None` (with
/// a stderr note) when unset or malformed — keeps backward compatibility for
/// chains that predate the air-gapped-root model. When present, ceremony
/// (key_introduce / key_revoke) envelopes must be signed by this key, which
/// turns fsck into a real PKI trust check.
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

fn verify_chain(
    events: &[EventRow],
    verifying_key: &Option<VerifyingKey>,
) -> Result<(u64, u64), (i64, String)> {
    let mut expected_prev = GENESIS_HASH.to_string();
    let mut verified: u64 = 0;
    let mut sig_verified: u64 = 0;

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

        let hash_bytes = hex::decode(&event.hash).unwrap();
        let sig_bytes = hex::decode(&event.signature)
            .map_err(|_| (event.id, "invalid signature hex".to_string()))?;
        let sig_array: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| (event.id, "signature wrong length".to_string()))?;
        let sig = Signature::from_bytes(&sig_array);

        if let Some(ref pubkey_hex) = event.signing_key_pubkey {
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
        } else if let Some(ref vk) = verifying_key {
            if vk.verify_strict(&hash_bytes, &sig).is_err() {
                return Err((event.id, "signature failed".to_string()));
            }
            sig_verified += 1;
        }

        expected_prev = event.hash.clone();
        verified += 1;
    }

    Ok((verified, sig_verified))
}

fn cmd_verify(
    conn: &rusqlite::Connection,
    verifying_key: &Option<VerifyingKey>,
    root_pubkey: &Option<VerifyingKey>,
) -> ExitCode {
    let events = match read_events(conn) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::from(2);
        }
    };

    if events.is_empty() {
        println!("✅ Ledger is empty (0 events) — nothing to verify.");
        return ExitCode::SUCCESS;
    }

    println!("Verifying {} events...", events.len());

    let mut healthy = true;
    match verify_chain(&events, verifying_key) {
        Ok((verified, sig_verified)) => {
            println!(
                "✅ Hash chain verified: {} events, all links valid.",
                verified
            );
            if sig_verified > 0 {
                println!(
                    "✅ Signatures verified: {}/{} events.",
                    sig_verified, verified
                );
            } else if verifying_key.is_none() {
                println!("ℹ️  Signature verification skipped (no --key provided).");
            }
        }
        Err((id, msg)) => {
            eprintln!("❌ Event #{}: {}", id, msg);
            healthy = false;
        }
    }

    // Root pubkey ceremony cross-check (when ROOT_PUBKEY_HEX is configured).
    // Verifies each key_introduce / key_revoke envelope was signed by root.
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
                    } else if !verify_ceremony_envelope(&event.target, &event.payload, sig, s) {
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

    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_fsck(
    conn: &rusqlite::Connection,
    verifying_key: &Option<VerifyingKey>,
    root_pubkey: &Option<VerifyingKey>,
) -> ExitCode {
    let events = match read_events(conn) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::from(2);
        }
    };

    if events.is_empty() {
        println!("✅ Ledger is empty (0 events) — nothing to check.");
        println!("✅ fsck complete — chain is HEALTHY");
        return ExitCode::SUCCESS;
    }

    println!("Running full ledger fsck ({} events)...\n", events.len());
    let mut healthy = true;

    // --- 1. Hash chain + signatures ---
    match verify_chain(&events, verifying_key) {
        Ok((verified, sig_verified)) => {
            println!("✅ Hash chain valid ({} events)", verified);
            if sig_verified > 0 {
                println!("✅ Signatures valid ({}/{})", sig_verified, verified);
            } else if verifying_key.is_none() {
                println!("ℹ️  Signature verification skipped (no --key provided)");
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

    // --- 4. Key trust chain ---
    let mut signers_seen: HashSet<String> = HashSet::new();
    let mut introduces: Vec<(i64, String)> = Vec::new();
    let mut revokes: Vec<(i64, String)> = Vec::new();
    let mut introduced_keys: HashSet<String> = HashSet::new();
    let mut revoked_keys: HashSet<String> = HashSet::new();
    let mut duplicate_introduces: Vec<String> = Vec::new();
    let mut revoked_key_uses: Vec<i64> = Vec::new();
    let mut envelope_failures: Vec<(i64, String)> = Vec::new();

    for event in &events {
        if let Some(ref pk) = event.signing_key_pubkey {
            signers_seen.insert(pk.clone());
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
                    // Cross-check: envelope's claimed signer must match the
                    // chain row's signing_key_pubkey (which is cryptographically
                    // bound by the chain signature from 4a). Without this, an
                    // attacker can forge a payload claiming any signer.
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

    println!("✅ Key trust chain:");
    for pk in &signers_seen {
        let short = &pk[..pk.len().min(12)];
        let status = if revoked_keys.contains(pk) {
            "REVOKED"
        } else {
            "active"
        };
        let intro = introduces.iter().find(|(_, p)| p == pk);
        let intro_str = match intro {
            Some((eid, _)) => format!("introduced at event #{}", eid),
            None => "implicit root".to_string(),
        };
        println!("   - {}... ({}, {})", short, status, intro_str);
    }
    println!(
        "   Key lifecycle: {} introduce(s), {} revoke(s)",
        introduces.len(),
        revokes.len()
    );

    // --- 5. Revoked key usage ---
    if revoked_key_uses.is_empty() {
        println!("✅ No revoked-key usage detected");
    } else {
        eprintln!(
            "❌ {} event(s) signed by revoked keys: {:?}",
            revoked_key_uses.len(),
            revoked_key_uses
        );
        healthy = false;
    }

    // --- 6. Duplicate introduces ---
    if !duplicate_introduces.is_empty() {
        eprintln!(
            "❌ Duplicate introduce events for: {:?}",
            duplicate_introduces
        );
        healthy = false;
    }

    // --- 7. Envelope signature validity ---
    if envelope_failures.is_empty() {
        if !introduces.is_empty() || !revokes.is_empty() {
            println!("✅ Ceremony envelope signatures valid");
        }
    } else {
        for (id, msg) in &envelope_failures {
            eprintln!("❌ Event #{}: {}", id, msg);
        }
        healthy = false;
    }

    // --- 8. Root pubkey cross-check (when ROOT_PUBKEY_HEX is configured) ---
    //
    // Each ceremony event (key_introduce / key_revoke) carries an envelope
    // signature from the *signer* whose pubkey is recorded inline (via
    // `introduced_by_pubkey_hex` / `revoked_by_pubkey_hex`). When operators
    // graduate to PKI, those ceremonies must be signed by the air-gapped root.
    // The check below enforces that policy whenever ROOT_PUBKEY_HEX is set.
    if let Some(root_vk) = root_pubkey {
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
    } else if !introduces.is_empty() || !revokes.is_empty() {
        println!("ℹ️  Root cross-check skipped (ROOT_PUBKEY_HEX not set)");
    }

    // --- Final verdict ---
    println!();
    if healthy {
        println!("✅ fsck complete — chain is HEALTHY");
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
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--key" && i + 1 < args.len() {
            key_path = Some(&args[i + 1]);
            i += 2;
        } else {
            eprintln!("Unknown argument: {}", args[i]);
            return ExitCode::from(2);
        }
    }

    let verifying_key: Option<VerifyingKey> = match key_path {
        Some(kp) => match load_verifying_key(kp) {
            Ok(vk) => Some(vk),
            Err(e) => {
                eprintln!("{}", e);
                return ExitCode::from(2);
            }
        },
        None => None,
    };

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
        "verify" => cmd_verify(&conn, &verifying_key, &root_pubkey),
        "fsck" => cmd_fsck(&conn, &verifying_key, &root_pubkey),
        _ => {
            eprintln!("Unknown command: {}", command);
            usage()
        }
    }
}
