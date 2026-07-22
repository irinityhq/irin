use base64::Engine as _;
use council_rs::governance::verify_outbox_signature;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

fn signed_fixture(canonical: &str) -> (Value, Value) {
    let key = SigningKey::from_bytes(&[9u8; 32]);
    let signature = key.sign(canonical.as_bytes());
    let kid = "sidecar-v1-integration";
    (
        json!({
            "envelope_json_canonical": canonical,
            "signature": {
                "alg": "Ed25519",
                "kid": kid,
                "value": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            }
        }),
        json!({
            "alg": "Ed25519",
            "kid": kid,
            "pubkey_b64": base64::engine::general_purpose::STANDARD
                .encode(key.verifying_key().as_bytes()),
        }),
    )
}

#[test]
fn verifies_exact_utf8_and_rejects_tampering() {
    let (mut directive, pubkey) = signed_fixture(r#"{"scope":"read_only","n":1}"#);
    assert!(verify_outbox_signature(&directive, &pubkey).verified);

    directive["envelope_json_canonical"] =
        Value::String(r#"{"scope":"mutated","n":1}"#.to_string());
    let tampered = verify_outbox_signature(&directive, &pubkey);
    assert!(!tampered.verified);
    assert_eq!(tampered.detail, "signature_mismatch");
}
