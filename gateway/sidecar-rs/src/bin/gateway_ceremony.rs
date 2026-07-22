// ==========================================================================
// gateway-ceremony — Air-gapped CLI for signing PKI ceremony envelopes.
//
// Two ceremony events drive the gateway's key lifecycle, and both can be
// pre-signed offline by an operator holding the air-gapped root key:
//
//   introduce — attest "this new pubkey is authorized for ledger signing"
//   revoke    — attest "this previously-introduced pubkey is now retired"
//
// Output is a JSON envelope matching the on-chain payload schema for
// `key_introduce` / `key_revoke` (see COUNCIL_GATEWAY_CONTRACT.md §
// "Key lifecycle events"). Operators copy the envelope onto the running
// gateway and submit it through the normal ledger admin path; the chain
// signature is added there. The root signing key never leaves the air-gap.
//
// Subcommands:
//
//   introduce --root-key <path> --new-key <path> --output <path>
//                                [--purpose ledger_signing]
//
//   revoke    --root-key <path> --revoke-key <path> --output <path>
//                                [--reason "<text>"]
//
//   verify    --root-pubkey <hex|@path> --input <path>
//
// All key files are 32-byte raw Ed25519 seeds (the same on-disk format the
// running sidecar uses; the `.pem` extension on `~/.irin/ledger_key.pem`
// is historical, not actually PEM).
// ==========================================================================

use clap::{Parser, Subcommand};
use ed25519_dalek::{SigningKey, VerifyingKey};
use gateway_sidecar::keymgmt::{self, CeremonyPurpose, KeyIntroducePayload, KeyRevokePayload};
use std::path::PathBuf;
use std::process::ExitCode;

const EVENT_KEY_INTRODUCE_TARGET: &str = "key_introduce";
const EVENT_KEY_REVOKE_TARGET: &str = "key_revoke";

#[derive(Parser)]
#[command(
    name = "gateway-ceremony",
    about = "Offline signer for gateway PKI ceremony envelopes (introduce / revoke)",
    long_about = "Sign air-gapped key_introduce / key_revoke envelopes for the gateway audit ledger.\n\
                  Output JSON matches the on-chain payload schema in COUNCIL_GATEWAY_CONTRACT.md.\n\
                  Run on an offline workstation that holds the root signing key."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign a key_introduce envelope authorizing a new ledger-signing key.
    Introduce {
        /// 32-byte raw Ed25519 seed of the air-gapped root signing key.
        #[arg(long, value_name = "PATH")]
        root_key: PathBuf,
        /// 32-byte raw Ed25519 seed of the new signing key being introduced.
        /// Only its public component is used; the seed is read so this CLI
        /// works even when only the keypair file is available.
        #[arg(long, value_name = "PATH")]
        new_key: PathBuf,
        /// Output path for the JSON envelope.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
        /// Ceremony purpose. Currently only `ledger_signing` is supported.
        #[arg(long, default_value = "ledger_signing")]
        purpose: String,
        /// Verify the produced envelope before exiting.
        #[arg(long)]
        verify: bool,
    },

    /// Sign a key_revoke envelope retiring a previously-introduced key.
    Revoke {
        /// 32-byte raw Ed25519 seed of the air-gapped root signing key.
        #[arg(long, value_name = "PATH")]
        root_key: PathBuf,
        /// 32-byte raw Ed25519 seed of the key being revoked. Only its
        /// public component is hashed into the envelope.
        #[arg(long, value_name = "PATH")]
        revoke_key: PathBuf,
        /// Output path for the JSON envelope.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
        /// Free-form reason recorded on-chain (e.g. "scheduled rotation").
        #[arg(long, default_value = "scheduled rotation")]
        reason: String,
        /// Verify the produced envelope before exiting.
        #[arg(long)]
        verify: bool,
    },

    /// Verify a previously-produced envelope against an expected signer.
    /// Reads the JSON file and checks the embedded envelope signature.
    Verify {
        /// Expected signer pubkey: 64-char hex, or `@<path>` to read a file.
        #[arg(long, value_name = "HEX_OR_@PATH")]
        root_pubkey: String,
        /// JSON envelope to verify (output of `introduce` or `revoke`).
        #[arg(long, value_name = "PATH")]
        input: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Key file helpers
// ---------------------------------------------------------------------------

fn read_seed_32(path: &PathBuf) -> Result<[u8; 32], String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read key file {}: {}", path.display(), e))?;
    if bytes.len() != 32 {
        return Err(format!(
            "key file {} must be exactly 32 raw bytes (got {})",
            path.display(),
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn load_signing_key(path: &PathBuf) -> Result<SigningKey, String> {
    let seed = read_seed_32(path)?;
    Ok(SigningKey::from_bytes(&seed))
}

fn load_pubkey_bytes(path: &PathBuf) -> Result<[u8; 32], String> {
    // The new_key / revoke_key arguments are seeds (the same on-disk format
    // as the root key) — derive the public component from the seed so the
    // operator only needs one file format to manage.
    let seed = read_seed_32(path)?;
    let sk = SigningKey::from_bytes(&seed);
    Ok(sk.verifying_key().to_bytes())
}

fn parse_pubkey_arg(arg: &str) -> Result<VerifyingKey, String> {
    // Accept either `@<path>` (treated as a 32-byte seed file we derive the
    // pubkey from) or a 64-char hex public key directly.
    let hex_str = if let Some(stripped) = arg.strip_prefix('@') {
        let path = PathBuf::from(stripped);
        let pk = load_pubkey_bytes(&path)?;
        hex::encode(pk)
    } else {
        arg.trim().to_string()
    };
    if hex_str.len() != 64 {
        return Err(format!(
            "expected 64-char hex pubkey (got {} chars)",
            hex_str.len()
        ));
    }
    let bytes = hex::decode(&hex_str).map_err(|e| format!("pubkey is not valid hex: {}", e))?;
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| format!("pubkey bytes do not form a valid Ed25519 point: {}", e))
}

fn parse_purpose(s: &str) -> Result<CeremonyPurpose, String> {
    match s {
        "ledger_signing" => Ok(CeremonyPurpose::LedgerSigning),
        other => Err(format!(
            "unsupported purpose `{}` (only `ledger_signing` is currently defined)",
            other
        )),
    }
}

// ---------------------------------------------------------------------------
// Output envelope (matches the on-chain payload schema)
// ---------------------------------------------------------------------------
//
// The wrapper carries the ledger event `target` plus the payload itself, so
// operators can pipe the file straight into the admin/ledger record path.
// Fields below mirror COUNCIL_GATEWAY_CONTRACT.md § "Key lifecycle events".

#[derive(serde::Serialize, serde::Deserialize)]
struct EnvelopeFile {
    target: String,
    payload: serde_json::Value,
}

fn write_output(path: &PathBuf, env: &EnvelopeFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(env)
        .map_err(|e| format!("failed to serialize envelope: {}", e))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create output dir {}: {}", parent.display(), e))?;
        }
    }
    std::fs::write(path, json.as_bytes())
        .map_err(|e| format!("cannot write output file {}: {}", path.display(), e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn cmd_introduce(
    root_key: &PathBuf,
    new_key: &PathBuf,
    output: &PathBuf,
    purpose: &str,
    verify_after: bool,
) -> ExitCode {
    let signer = match load_signing_key(root_key) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("Error loading root key: {}", e);
            return ExitCode::from(2);
        }
    };
    let new_pk_bytes = match load_pubkey_bytes(new_key) {
        Ok(pk) => pk,
        Err(e) => {
            eprintln!("Error loading new key: {}", e);
            return ExitCode::from(2);
        }
    };
    let purpose = match parse_purpose(purpose) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            return ExitCode::from(2);
        }
    };

    let payload: KeyIntroducePayload = keymgmt::sign_introduce(&signer, &new_pk_bytes, purpose);

    if verify_after && !keymgmt::verify_introduce(&payload, &signer.verifying_key()) {
        eprintln!("❌ Self-verification failed — refusing to write output");
        return ExitCode::from(1);
    }

    let envelope = EnvelopeFile {
        target: EVENT_KEY_INTRODUCE_TARGET.to_string(),
        payload: serde_json::to_value(&payload).expect("payload serializes"),
    };
    if let Err(e) = write_output(output, &envelope) {
        eprintln!("Error writing output: {}", e);
        return ExitCode::from(2);
    }

    println!("✅ Wrote key_introduce envelope: {}", output.display());
    println!("   New pubkey:   {}", payload.new_pubkey_hex);
    println!("   Signed by:    {}", payload.introduced_by_pubkey_hex);
    println!("   Purpose:      {}", payload.purpose);
    if verify_after {
        println!("✅ Envelope signature verified against root verifying key.");
    }
    ExitCode::SUCCESS
}

fn cmd_revoke(
    root_key: &PathBuf,
    revoke_key: &PathBuf,
    output: &PathBuf,
    reason: &str,
    verify_after: bool,
) -> ExitCode {
    let signer = match load_signing_key(root_key) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("Error loading root key: {}", e);
            return ExitCode::from(2);
        }
    };
    let target_pk_bytes = match load_pubkey_bytes(revoke_key) {
        Ok(pk) => pk,
        Err(e) => {
            eprintln!("Error loading key to revoke: {}", e);
            return ExitCode::from(2);
        }
    };
    let revoked_pubkey_hex = hex::encode(target_pk_bytes);

    let payload: KeyRevokePayload = keymgmt::sign_revoke(&signer, &revoked_pubkey_hex, reason);

    if verify_after && !keymgmt::verify_revoke(&payload, &signer.verifying_key()) {
        eprintln!("❌ Self-verification failed — refusing to write output");
        return ExitCode::from(1);
    }

    let envelope = EnvelopeFile {
        target: EVENT_KEY_REVOKE_TARGET.to_string(),
        payload: serde_json::to_value(&payload).expect("payload serializes"),
    };
    if let Err(e) = write_output(output, &envelope) {
        eprintln!("Error writing output: {}", e);
        return ExitCode::from(2);
    }

    println!("✅ Wrote key_revoke envelope: {}", output.display());
    println!("   Revoked pubkey: {}", payload.revoked_pubkey_hex);
    println!("   Signed by:      {}", payload.revoked_by_pubkey_hex);
    println!("   Reason:         {}", payload.reason);
    if verify_after {
        println!("✅ Envelope signature verified against root verifying key.");
    }
    ExitCode::SUCCESS
}

fn cmd_verify(root_pubkey_arg: &str, input: &PathBuf) -> ExitCode {
    let vk = match parse_pubkey_arg(root_pubkey_arg) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("Error parsing --root-pubkey: {}", e);
            return ExitCode::from(2);
        }
    };
    let raw = match std::fs::read_to_string(input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", input.display(), e);
            return ExitCode::from(2);
        }
    };
    let env: EnvelopeFile = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing envelope JSON: {}", e);
            return ExitCode::from(2);
        }
    };

    let ok = match env.target.as_str() {
        EVENT_KEY_INTRODUCE_TARGET => {
            let payload: KeyIntroducePayload = match serde_json::from_value(env.payload) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error parsing key_introduce payload: {}", e);
                    return ExitCode::from(2);
                }
            };
            keymgmt::verify_introduce(&payload, &vk)
        }
        EVENT_KEY_REVOKE_TARGET => {
            let payload: KeyRevokePayload = match serde_json::from_value(env.payload) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error parsing key_revoke payload: {}", e);
                    return ExitCode::from(2);
                }
            };
            keymgmt::verify_revoke(&payload, &vk)
        }
        other => {
            eprintln!("Error: unknown envelope target `{}`", other);
            return ExitCode::from(2);
        }
    };

    if ok {
        println!("✅ Envelope signature valid (target={})", env.target);
        ExitCode::SUCCESS
    } else {
        eprintln!("❌ Envelope signature INVALID (target={})", env.target);
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Introduce {
            root_key,
            new_key,
            output,
            purpose,
            verify,
        } => cmd_introduce(root_key, new_key, output, purpose, *verify),
        Cmd::Revoke {
            root_key,
            revoke_key,
            output,
            reason,
            verify,
        } => cmd_revoke(root_key, revoke_key, output, reason, *verify),
        Cmd::Verify { root_pubkey, input } => cmd_verify(root_pubkey, input),
    }
}
