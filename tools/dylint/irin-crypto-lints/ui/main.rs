//! UI fixtures for IRIN crypto dylints.
//! Warnings are expected; clean helpers exist for contrast.

// --- no_debug_on_signing_key_types ---

#[derive(Debug, Clone)]
struct SigningKey([u8; 32]);

#[derive(Debug)]
struct LedgerKey {
    bytes: [u8; 32],
}

// Field type ends in SigningKey even if the container name does not match.
#[derive(Debug)]
struct Holder {
    key: SigningKey,
}

// Clean: no Debug derive on key material.
struct SecretKey([u8; 32]);

// Clean: ordinary type may derive Debug.
#[derive(Debug)]
struct Config {
    name: String,
}

// --- prefer_subtle_ct_eq ---

#[derive(PartialEq)]
struct PrivateKey([u8; 32]);

fn verify_mac(a: &[u8], b: &[u8]) -> bool {
    a == b
}

fn compare_tokens(left: Vec<u8>, right: Vec<u8>) -> bool {
    left == right
}

// Clean: non-sensitive function name.
fn format_lengths(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
}

// Clean: sensitive name but non-byte comparison.
fn verify_count(a: usize, b: usize) -> bool {
    a == b
}

fn main() {
    let _ = SigningKey([0; 32]);
    let _ = LedgerKey { bytes: [0; 32] };
    let _ = Holder {
        key: SigningKey([0; 32]),
    };
    let _ = SecretKey([0; 32]);
    let _ = Config {
        name: String::new(),
    };
    let _ = PrivateKey([0; 32]);
    let _ = verify_mac(b"a", b"b");
    let _ = compare_tokens(vec![1], vec![2]);
    let _ = format_lengths(b"a", b"b");
    let _ = verify_count(1, 2);
}
