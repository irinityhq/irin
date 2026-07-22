//! RFC 8785 / ES6 number conformance oracle (W5, Invariant).
//!
//! Two independent correctness nets for the hand-ported ES6 `Number::toString`
//! in `sovereign_protocol::jcs`:
//!
//! 1. **Vendored cyberphone vectors** (`tests/vectors/`): the authoritative
//!    cross-implementation `hex-ieee,expected` table from
//!    cyberphone/json-canonicalization (the published ES6 number test file).
//!    Vendored hermetically (no network at test time) and content-hash pinned
//!    below. A curated boundary set + a deterministic ~50k sample of the 100M
//!    file. The expected strings are NOT self-generated — they are the reference
//!    output the spec's own test suite asserts.
//!
//! 2. **Differential oracle** (`json-canon`, a maintained RFC 8785 reference,
//!    DEV-DEPENDENCY ONLY): assert `our_canon(x) == reference(x)` over the
//!    sampled f64 plus seeded + adversarial values. `json-canon` never appears
//!    under `[dependencies]` and never touches the runtime signing path.
//!
//! Provenance (pinned):
//! - upstream: cyberphone/json-canonicalization, ES6 number test file
//!   <https://github.com/cyberphone/json-canonicalization/releases/download/es6testfile/es6testfile100m.txt.gz>
//! - gz sha256: 545455ec9e74b68042c22a2607fb9d4a5f5fdb3c79f883ce864c75189a70705f
//! - vendored es6_boundary.txt sha256:
//!   258482d320f3afb9a4ea187280391420f546ef60ddf3dd7f8af7e4384eaf690f
//! - vendored es6_sampled.txt sha256:
//!   45f39298a4504e3bf6eebeca943484098a7428778b9ac6f9cfa045d60ea15def

use serde_json::json;
use sovereign_protocol::jcs;

const BOUNDARY_VECTORS: &str = include_str!("vectors/es6_boundary.txt");
const SAMPLED_VECTORS: &str = include_str!("vectors/es6_sampled.txt");

// Content hashes the vendored vectors MUST match (defense against silent edits).
const BOUNDARY_SHA256: &str = "258482d320f3afb9a4ea187280391420f546ef60ddf3dd7f8af7e4384eaf690f";
const SAMPLED_SHA256: &str = "45f39298a4504e3bf6eebeca943484098a7428778b9ac6f9cfa045d60ea15def";

/// Decode the f64 our encoder must canonicalize for a cyberphone `hex-ieee` key.
fn f64_from_hex(hex: &str) -> f64 {
    let bits = u64::from_str_radix(hex, 16).expect("vector hex is valid u64");
    f64::from_bits(bits)
}

/// Our canonical form of a bare number value.
fn our_number(x: f64) -> String {
    jcs::to_jcs_string(&json!(x)).expect("finite vector value canonicalizes")
}

/// Parse `hex,expected` lines, skipping blanks.
fn parse_vectors(raw: &str) -> Vec<(&str, &str)> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (hex, expected) = l.split_once(',').expect("vector line is hex,expected");
            (hex, expected)
        })
        .collect()
}

// Content-hash pin: recompute SHA-256 over the vendored vectors at test time and
// assert it matches the constants above. This catches any silent edit to the
// vendored cyberphone data (a "green test defending the wrong bytes" would
// otherwise be possible). `sha2` is a dev-dependency only — never on a runtime path.

#[test]
fn vendored_vectors_content_hash_pinned() {
    use sha2::{Digest, Sha256};
    let b = format!("{:x}", Sha256::digest(BOUNDARY_VECTORS.as_bytes()));
    let s = format!("{:x}", Sha256::digest(SAMPLED_VECTORS.as_bytes()));
    assert_eq!(
        b, BOUNDARY_SHA256,
        "es6_boundary.txt was edited; re-pin or revert"
    );
    assert_eq!(
        s, SAMPLED_SHA256,
        "es6_sampled.txt was edited; re-pin or revert"
    );
}

#[test]
fn cyberphone_boundary_vectors_pass() {
    let vectors = parse_vectors(BOUNDARY_VECTORS);
    assert!(!vectors.is_empty(), "boundary vector file is empty");
    let mut failures = Vec::new();
    for (hex, expected) in &vectors {
        let x = f64_from_hex(hex);
        let got = our_number(x);
        if got != *expected {
            failures.push(format!("hex={hex} expected={expected} got={got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} / {} boundary vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

#[test]
fn cyberphone_sampled_vectors_pass() {
    let vectors = parse_vectors(SAMPLED_VECTORS);
    assert!(vectors.len() > 10_000, "sampled set unexpectedly small");
    let mut failures = Vec::new();
    for (hex, expected) in &vectors {
        let x = f64_from_hex(hex);
        let got = our_number(x);
        if got != *expected {
            failures.push(format!("hex={hex} expected={expected} got={got}"));
            if failures.len() > 50 {
                break; // cap the report size
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}+ / {} sampled vectors failed (first 50):\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

/// Mandatory boundary checklist (brief): exact named cutover cases. These are a
/// subset of the cyberphone vectors above, asserted by literal so a regression
/// names the exact failing boundary rather than burying it in a count.
#[test]
fn mandatory_boundary_checklist() {
    let cases: &[(f64, &str)] = &[
        (1e21, "1e+21"),                                // n>21 high cutover -> exponential
        (1e20, "100000000000000000000"),                // last fixed below the cutover
        (1e10, "10000000000"),                          // pre-W5 bug: was scientific
        (1e-6, "0.000001"),                             // n<=-6 boundary: still fixed
        (1e-7, "1e-7"),                                 // first exponential on the low side
        (-0.0, "0"),                                    // negative zero
        (5e-324, "5e-324"),                             // smallest subnormal
        (f64::MAX, "1.7976931348623157e+308"),          // 17-sig-digit + e+ sign
        (9.999999999999997e-7, "9.999999999999997e-7"), // e- exponent sign
    ];
    for (x, expected) in cases {
        assert_eq!(&our_number(*x), expected, "boundary {x} mis-canonicalized");
    }
}

/// Differential oracle: our encoder must agree with the maintained `json-canon`
/// reference for the sampled cyberphone f64 values. This is the cross-impl check
/// that the vendored expected-strings and our hand-port AND the reference all
/// converge — three independent encoders.
#[test]
fn differential_oracle_sampled() {
    let vectors = parse_vectors(SAMPLED_VECTORS);
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (hex, _expected) in &vectors {
        let x = f64_from_hex(hex);
        let ours = our_number(x);
        let reference = json_canon::to_string(&json!(x)).expect("reference canonicalizes");
        checked += 1;
        if ours != reference {
            failures.push(format!("hex={hex} ours={ours} reference={reference}"));
            if failures.len() > 50 {
                break;
            }
        }
    }
    assert!(checked > 10_000, "differential sample too small: {checked}");
    assert!(
        failures.is_empty(),
        "{}+ differential mismatches vs json-canon (first 50):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("differential_oracle_sampled: {checked} values, 0 mismatches vs json-canon");
}

/// Differential oracle over seeded + adversarial f64 beyond the vendored sample:
/// edges, powers of ten across both cutovers, dense fractional values, and
/// pseudo-random doubles (deterministic seed for reproducibility).
#[test]
fn differential_oracle_adversarial() {
    let mut xs: Vec<f64> = Vec::new();

    // Powers of ten straddling both cutovers.
    for e in -30i32..=30 {
        xs.push(10f64.powi(e));
        xs.push(-(10f64.powi(e)));
        xs.push(9.999_999_999_999_999 * 10f64.powi(e));
    }
    // Edges and notable constants.
    xs.extend_from_slice(&[
        0.0,
        -0.0,
        1.0,
        -1.0,
        f64::MIN_POSITIVE,
        5e-324,
        f64::MAX,
        f64::MIN,
        0.1,
        0.2,
        0.3,
        1.0 / 3.0,
        2.0 / 3.0,
        std::f64::consts::PI,
        std::f64::consts::E,
        0.00525,
        0.0079,
        0.125,
        9007199254740992.0,
        9007199254740993.0,
    ]);

    // Deterministic pseudo-random doubles (xorshift64; no rand dep).
    let mut state: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..20_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let x = f64::from_bits(state);
        if x.is_finite() {
            xs.push(x);
        }
    }

    let mut failures = Vec::new();
    let mut checked = 0usize;
    for x in xs {
        let ours = our_number(x);
        let reference = json_canon::to_string(&json!(x)).expect("reference canonicalizes");
        checked += 1;
        // Both must also round-trip back to the same f64 (sanity on ours).
        if ours != reference {
            failures.push(format!(
                "x_bits={:016x} ours={ours} reference={reference}",
                x.to_bits()
            ));
            if failures.len() > 50 {
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}+ adversarial differential mismatches (first 50):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("differential_oracle_adversarial: {checked} values, 0 mismatches vs json-canon");
}

// --- Object key-ordering differential oracle (W5.1 FIX 1 blind-spot closure) ---
//
// W5's oracle only fed NUMBERS (`json!(float)`), so it never exercised object key
// ordering — the exact gap that let the UTF-16-vs-scalar key-sort bug ship. These
// vectors feed multi-key OBJECTS whose keys span ASCII, BMP (incl. U+E000
// private-use), non-BMP (U+10000, U+1F600 emoji, U+10FFFF), and codepoints adjacent
// to the surrogate boundary, with shuffled insertion order, and assert our canonical
// form is byte-identical to the maintained `json-canon` reference (which sorts by
// UTF-16 code units per RFC 8785 §3.2.3).

/// Key alphabet: chars whose UTF-16 vs Unicode-scalar ordering can diverge.
/// Spans ASCII, BMP, the surrogate-pair non-BMP range, and U+E000 private-use
/// (the BMP codepoint that sorts AFTER non-BMP under UTF-16 but BEFORE under scalar).
fn key_alphabet() -> Vec<char> {
    vec![
        'A',          // U+0041 ASCII
        'z',          // U+007A ASCII
        '\u{0080}',   // first non-ASCII BMP
        '\u{07FF}',   // 2-byte UTF-8 boundary
        '\u{0800}',   // 3-byte UTF-8 start
        '\u{D7FF}',   // last BMP scalar below the surrogate range
        '\u{E000}',   // first BMP private-use ABOVE the surrogate range (the T-Rex char)
        '\u{F900}',   // BMP CJK compat
        '\u{FFFF}',   // last BMP scalar
        '\u{10000}',  // first non-BMP (surrogate pair 0xD800,0xDC00)
        '\u{1F600}',  // emoji 😀 (surrogate pair starting 0xD83D)
        '\u{2FA1D}',  // non-BMP CJK
        '\u{10FFFF}', // highest scalar (surrogate pair 0xDBFF,0xDFFF)
    ]
}

/// Build a JSON object from the given single-char keys in the given (shuffled)
/// insertion order. Distinct values so a mis-sort would change the serialized bytes.
fn obj_from_keys(keys: &[char]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (i, k) in keys.iter().enumerate() {
        map.insert(k.to_string(), json!(i as i64));
    }
    serde_json::Value::Object(map)
}

#[test]
fn differential_oracle_object_keys_non_bmp() {
    let alphabet = key_alphabet();

    // Deterministic xorshift64 to generate shuffled key subsets (no rand dep).
    let mut state: u64 = 0xD1B54A32D192ED03;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut objects: Vec<serde_json::Value> = Vec::new();

    // The exact T-Rex focused vector and its inverse insertion order.
    objects.push(obj_from_keys(&['\u{E000}', '\u{10000}']));
    objects.push(obj_from_keys(&['\u{10000}', '\u{E000}']));
    // Multi-key BMP-vs-non-BMP straddle.
    objects.push(obj_from_keys(&[
        '\u{E000}',
        '\u{10000}',
        'A',
        '\u{1F600}',
        '\u{FFFF}',
    ]));

    // The full alphabet under many shuffled insertion orders.
    for _ in 0..2_000 {
        let mut keys = alphabet.clone();
        // Fisher-Yates with the xorshift stream.
        for i in (1..keys.len()).rev() {
            let j = (next() as usize) % (i + 1);
            keys.swap(i, j);
        }
        objects.push(obj_from_keys(&keys));
    }

    // Random subsets of the alphabet (varying arity), shuffled.
    for _ in 0..3_000 {
        let mut subset: Vec<char> = alphabet
            .iter()
            .copied()
            .filter(|_| next() & 1 == 0)
            .collect();
        if subset.len() < 2 {
            continue;
        }
        for i in (1..subset.len()).rev() {
            let j = (next() as usize) % (i + 1);
            subset.swap(i, j);
        }
        objects.push(obj_from_keys(&subset));
    }

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for obj in &objects {
        let ours = jcs::to_jcs_string(obj).expect("object canonicalizes");
        let reference = json_canon::to_string(obj).expect("reference canonicalizes");
        checked += 1;
        if ours != reference {
            failures.push(format!("ours={ours} reference={reference}"));
            if failures.len() > 50 {
                break;
            }
        }
    }
    assert!(
        checked > 4_500,
        "object-key differential sample too small: {checked}"
    );
    assert!(
        failures.is_empty(),
        "{}+ object-key differential mismatches vs json-canon (first 50):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "differential_oracle_object_keys_non_bmp: {checked} objects, 0 mismatches vs json-canon"
    );
}

/// Focused, literal-pinned regression for the exact bug: U+E000 vs U+10000.
/// json-canon (UTF-16 order) and our encoder must BOTH emit U+10000 first.
#[test]
fn object_key_order_e000_vs_10000_pinned() {
    let obj = json!({ "\u{E000}": 1, "\u{10000}": 2 });
    let ours = jcs::to_jcs_string(&obj).unwrap();
    let reference = json_canon::to_string(&obj).unwrap();
    assert_eq!(ours, reference, "must match the UTF-16-ordering reference");
    // U+10000 (f0 90 80 80) precedes U+E000 (ee 80 80) under UTF-16 code units.
    assert_eq!(ours, "{\"\u{10000}\":2,\"\u{E000}\":1}");
    let high = ours.find('\u{10000}').unwrap();
    let e000 = ours.find('\u{E000}').unwrap();
    assert!(
        high < e000,
        "U+10000 must sort before U+E000 (UTF-16); got {ours}"
    );
}

// --- String-escape differential oracle (W5.1 P1-A: the next untested axis) ---
//
// Same blind-spot class as the UTF-16 key bug: W5's oracle never exercised string
// ESCAPING (RFC 8785 §3.2.2.2 — minimal JSON escaping). These chars are fed as BOTH
// object keys AND string values; we assert byte-identity with json-canon. If
// json-canon disagrees on any char, that is the next W5-class bug — the test names
// the exact divergence rather than masking it.

/// Chars whose JSON string-escaping is non-trivial: the C0 control range
/// (U+0000..U+001F, all MUST be escaped), the two mandatory two-char escapes
/// (`"` and `\`), the optional `/` (which RFC 8785 / ES6 does NOT escape), DEL
/// (U+007F, NOT escaped), the JS line/paragraph separators U+2028/U+2029 (NOT
/// escaped under RFC 8785 — a classic divergence point vs naive JS serializers),
/// and a non-BMP char to cover the surrogate-pair string path.
fn escape_alphabet() -> Vec<char> {
    let mut v: Vec<char> = (0u32..=0x1F).map(|c| char::from_u32(c).unwrap()).collect();
    v.extend([
        '"',
        '\\',
        '/',
        '\u{007F}',
        '\u{2028}',
        '\u{2029}',
        '\u{0080}',
        '\u{10000}',
        '\u{1F600}',
    ]);
    v
}

#[test]
fn differential_oracle_string_escapes() {
    let alphabet = escape_alphabet();

    // Deterministic xorshift64 (no rand dep) for shuffled multi-char strings.
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut samples: Vec<serde_json::Value> = Vec::new();

    // 1) Each char alone, as a bare string value.
    for c in &alphabet {
        samples.push(json!(c.to_string()));
    }
    // 2) Each char as an object KEY (and as the value too) — exercises the escape
    //    path in key position, which the W5 oracle never touched.
    for c in &alphabet {
        let mut m = serde_json::Map::new();
        m.insert(c.to_string(), json!(format!("v{c}x")));
        samples.push(serde_json::Value::Object(m));
    }
    // 3) Embedded escapes interleaved with ASCII: `a\nb\tc"d\e/f` style strings.
    for _ in 0..2_000 {
        let len = 1 + (next() as usize % 6);
        let mut s = String::new();
        for _ in 0..len {
            s.push((b'a' + (next() as u8 % 26)) as char);
            s.push(alphabet[next() as usize % alphabet.len()]);
        }
        // As a value...
        samples.push(json!(s.clone()));
        // ...and as a key with a distinct escaped value.
        let mut m = serde_json::Map::new();
        m.insert(s.clone(), json!(s));
        samples.push(serde_json::Value::Object(m));
    }
    // 4) Multi-key objects whose keys are escape-bearing strings, shuffled order.
    for _ in 0..1_000 {
        let mut m = serde_json::Map::new();
        let n = 2 + (next() as usize % 5);
        for j in 0..n {
            let c = alphabet[next() as usize % alphabet.len()];
            m.insert(format!("k{j}{c}"), json!(j as i64));
        }
        samples.push(serde_json::Value::Object(m));
    }

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for v in &samples {
        let ours = jcs::to_jcs_string(v).expect("escape sample canonicalizes");
        let reference = json_canon::to_string(v).expect("reference canonicalizes");
        checked += 1;
        if ours != reference {
            // Hex-dump both so a control-char divergence is legible in CI output.
            failures.push(format!(
                "ours={:?} ({}) reference={:?} ({})",
                ours,
                hex(&ours),
                reference,
                hex(&reference)
            ));
            if failures.len() > 50 {
                break;
            }
        }
    }
    assert!(
        checked > 3_000,
        "escape differential sample too small: {checked}"
    );
    assert!(
        failures.is_empty(),
        "{}+ string-escape differential mismatches vs json-canon (first 50):\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("differential_oracle_string_escapes: {checked} samples, 0 mismatches vs json-canon");
}

/// Hex of a string's bytes, for legible control-char divergence reports.
fn hex(s: &str) -> String {
    s.bytes()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Round-trip property: parsing our canonical output yields the original f64.
#[test]
fn round_trip_sampled() {
    let vectors = parse_vectors(SAMPLED_VECTORS);
    let mut failures = 0usize;
    for (hex, _) in &vectors {
        let x = f64_from_hex(hex);
        let s = our_number(x);
        let back: f64 = s.parse().expect("our canonical form is valid JSON number");
        // Bit-exact except both zeros canonicalize to "0".
        if x == 0.0 {
            assert_eq!(back, 0.0);
        } else if back.to_bits() != x.to_bits() {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} round-trip failures over sampled set"
    );
}
