//! JSON Canonicalization Scheme (RFC 8785) for the IRIN wire spine.
//!
//! RFC 8785 requires lexicographic sorting of object keys by UTF-16 code units
//! (ECMAScript string ordering, §3.2.3) — re-sorted explicitly in
//! [`value_to_jcs_bytes`] because `serde_json::Value`'s `BTreeMap` orders by
//! Unicode-scalar (UTF-8) order, which diverges for non-BMP keys — plus no
//! insignificant whitespace, and number formatting identical to the ECMAScript
//! `Number::toString` algorithm
//! (RFC 8785 §3.2.2.3, ECMA-262 7.1.12.1) — the *shortest* decimal that
//! round-trips to the same IEEE-754 double.
//!
//! ## External-verify guarantee
//! These bytes are the exact preimage Ed25519-signs for every escalation,
//! directive, and capability token. A third party holding the published pubkey
//! re-canonicalizes with any conformant RFC 8785 library and verifies the
//! signature. The number formatting therefore MUST be byte-identical to a
//! conformant ES6 serializer — proven against the cyberphone/json-canonicalization
//! cross-implementation vectors (`tests/jcs_conformance.rs`, vectors vendored and
//! content-hash pinned in `tests/vectors/`).
//!
//! ## Number conformance (W5, Invariant, Q1=Option B)
//! Integers use the exact i64/u64 form. Floats get their shortest round-trip
//! digits from `ryu`, then [`canonical_jcs_float`] re-emits them per the four ES6
//! branch rules. Exponential cutover is at decimal exponent `n > 21` and `n <= -6`
//! (NOT the pre-W5 1e10 / 1e-9 thresholds, which were both non-conformant).
//!
//! ## Operational notes (W5.1)
//! - RFC 8785 applies NO Unicode normalization, so NFC-vs-NFD spellings of a key
//!   are intentionally DISTINCT keys (spec-correct — do NOT "fix" by normalizing).
//! - Do NOT enable the `serde_json` `arbitrary_precision` feature without re-running
//!   the number-port conformance oracle: it changes `Number::as_i64`/`as_f64`
//!   dispatch and would silently alter the signed-byte number formatting.

use serde::Serialize;
use serde_json::Error;

/// Error surface for canonicalization. Distinct from `serde_json::Error` so the
/// non-finite guard (defense-in-depth at the typed-value boundary) is explicit
/// at call sites and cannot be confused with a malformed-JSON parse error.
#[derive(Debug)]
pub enum JcsError {
    /// A `serde_json` (de)serialization error from the underlying value walk
    /// or the strict-path raw parse.
    Serde(Error),
    /// A non-finite float (`NaN`, `+Infinity`, `-Infinity`) reached
    /// canonicalization. RFC 8785 / JSON cannot represent these; a conformant
    /// serializer MUST error rather than emit. See [`to_jcs_bytes`] for why this
    /// is defense-in-depth rather than the primary control.
    NonFinite,
    /// Raw JSON carried duplicate keys within the same object (RFC 8785 §3.2.1),
    /// rejected by the strict path before any signed bytes are produced.
    DuplicateKeys,
}

impl std::fmt::Display for JcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JcsError::Serde(e) => write!(f, "JCS serialization error: {e}"),
            JcsError::NonFinite => write!(
                f,
                "non-finite float (NaN/Infinity) cannot be canonicalized per RFC 8785"
            ),
            JcsError::DuplicateKeys => {
                write!(f, "duplicate object keys per RFC 8785 §3.2.1")
            }
        }
    }
}

impl std::error::Error for JcsError {}

impl From<Error> for JcsError {
    fn from(e: Error) -> Self {
        JcsError::Serde(e)
    }
}

/// Serializes a serializable type to JSON Canonicalization Scheme (JCS) bytes.
///
/// ## Non-finite guard (defense-in-depth)
/// `serde_json` SILENTLY collapses every non-finite float to JSON `null`
/// (`to_value`, `to_string`, `to_vec`, `to_writer` all return `Ok("null")` —
/// verified by probe, W5). By the time we hold a `serde_json::Value`, every
/// `Number` is finite *by construction* (`Number::from_f64` returns `None` for
/// non-finite), so a finite-check inside the number formatter would be dead code.
/// The real exposure is the `to_value` boundary itself: a `NaN`-valued field of a
/// signed struct would otherwise become a silent `null` inside a signed artifact.
/// We therefore validate the TYPED value for non-finite floats up front with
/// the `finite_check` serializer and return [`JcsError::NonFinite`] before any
/// bytes are produced. Downstream `Value`-typed call sites (e.g. the directive payload
/// grafted in gateway `dispatcher.rs`) must still guard their float inserts —
/// once a `NaN` has collapsed to `Null` in a `Value`, it is indistinguishable
/// from an intentional `null` here.
///
/// ## Single-pass requirement (W5.1)
/// The non-finite check necessarily walks the typed `T` (the only point where
/// `NaN`/`Inf` is still distinguishable — `to_value` collapses it to `Null`,
/// confirmed by probe), and `to_value` then walks `T` a SECOND time to build the
/// signed `Value`. The contract is: a value's `Serialize` impl MUST be pure —
/// the same tree on every pass — so the tree the finite-check validated is the
/// tree that gets signed. Derived `Serialize` impls satisfy this; a hand-written
/// stateful `Serialize` would violate it. We cannot hold a single materialized
/// tree across both checks (that tree could no longer carry the non-finite
/// marker), so a `debug_assert` re-serializes and asserts determinism to surface
/// a non-pure impl in tests/CI without any release-build cost.
///
/// CONVENTION: signed types MUST `#[derive(Serialize)]`, never hand-roll a
/// `Serialize` impl whose emitted tree can vary across passes.
pub fn to_jcs_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, JcsError> {
    check_finite(value)?;
    let canonical_value = serde_json::to_value(value)?;
    debug_assert!(
        serde_json::to_value(value).ok().as_ref() == Some(&canonical_value),
        "Serialize impl is non-deterministic across passes; the finite-checked \
         tree differs from the signed tree (W5.1 single-pass requirement)"
    );
    Ok(value_to_jcs_bytes(&canonical_value))
}

/// Serializes a serializable type to a JCS string.
pub fn to_jcs_string<T: Serialize>(value: &T) -> Result<String, JcsError> {
    let bytes = to_jcs_bytes(value)?;
    Ok(String::from_utf8(bytes).expect("JCS output is valid UTF-8 by construction"))
}

/// Strict entry for raw attacker-controlled JSON (e.g. escalation/directive
/// payloads). Rejects duplicate keys per RFC 8785 §3.2.1 before any signing
/// path can produce bytes from them. Callers that ingest untrusted wire JSON
/// for signed envelopes MUST use this (or equivalent strict parse) before
/// constructing values passed to `to_jcs_*`.
///
/// This strict path is called in production:
/// `dispatcher.rs:parse_proposal_body()` (gateway `sidecar-rs/src/watch/dispatcher.rs:2537`)
/// routes raw council-response JSON through `to_jcs_bytes_strict()` before any
/// signing occurs, preventing last-wins duplicate-key collapse.
pub fn to_jcs_bytes_strict(raw_json: &str) -> Result<Vec<u8>, JcsError> {
    if has_duplicate_keys(raw_json) {
        return Err(JcsError::DuplicateKeys);
    }
    let v: serde_json::Value = serde_json::from_str(raw_json)?;
    // A parsed `Value` cannot hold a non-finite Number (serde_json rejects
    // `NaN`/`Infinity` tokens in JSON text), so no finite-check is needed here.
    Ok(value_to_jcs_bytes(&v))
}

// --- internal: custom JCS walk (keys re-sorted by UTF-16 code units; numbers RFC 8785) ---

fn value_to_jcs_bytes(v: &serde_json::Value) -> Vec<u8> {
    match v {
        serde_json::Value::Object(map) => {
            let mut out = Vec::with_capacity(128);
            out.push(b'{');
            let mut first = true;
            // RFC 8785 §3.2.3 sorts keys by ECMAScript string ordering = UTF-16
            // code units, NOT Rust's `String::cmp` (Unicode-scalar / UTF-8 order).
            // They DIVERGE for non-BMP keys (>= U+10000): a non-BMP scalar is a
            // surrogate pair starting 0xD800..0xDBFF, which sorts before BMP
            // private-use 0xE000 under UTF-16 but after it under scalar order
            // (e.g. keys U+10000 vs U+E000). `serde_json`'s BTreeMap pre-sorts by
            // scalar order, so we MUST re-sort by UTF-16 code units here or the
            // signed bytes diverge from a conformant external verifier.
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(k1, _), (k2, _)| k1.encode_utf16().cmp(k2.encode_utf16()));
            for (k, val) in entries {
                if !first {
                    out.push(b',');
                }
                first = false;
                // key as escaped JSON string (serde guarantees correct)
                let kjson = serde_json::to_vec(&serde_json::Value::String(k.clone())).unwrap();
                out.extend_from_slice(&kjson);
                out.push(b':');
                out.extend_from_slice(&value_to_jcs_bytes(val));
            }
            out.push(b'}');
            out
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(64);
            out.push(b'[');
            let mut first = true;
            for val in arr {
                if !first {
                    out.push(b',');
                }
                first = false;
                out.extend_from_slice(&value_to_jcs_bytes(val));
            }
            out.push(b']');
            out
        }
        serde_json::Value::Number(n) => canonical_jcs_number(n),
        serde_json::Value::String(s) => {
            serde_json::to_vec(&serde_json::Value::String(s.clone())).unwrap()
        }
        serde_json::Value::Bool(b) => {
            if *b {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            }
        }
        serde_json::Value::Null => b"null".to_vec(),
    }
}

/// Canonicalize a `serde_json::Number` per RFC 8785 §3.2.2.3.
///
/// Integers that fit i64/u64 take the exact decimal form. Everything else is a
/// finite f64 (a `Number` is finite by construction) and goes through the ES6
/// `Number::toString` hand-port in [`canonical_jcs_float`].
fn canonical_jcs_number(n: &serde_json::Number) -> Vec<u8> {
    if let Some(i) = n.as_i64() {
        return canonical_jcs_integer_i64(i);
    }
    if let Some(u) = n.as_u64() {
        return canonical_jcs_integer_u64(u);
    }
    // Remaining case: a non-integer (or out-of-i64/u64-range) finite f64.
    let f = n
        .as_f64()
        .expect("serde_json::Number is finite by construction");
    canonical_jcs_float(f)
}

#[inline]
fn canonical_jcs_integer_i64(i: i64) -> Vec<u8> {
    i.to_string().into_bytes()
}

#[inline]
fn canonical_jcs_integer_u64(u: u64) -> Vec<u8> {
    u.to_string().into_bytes()
}

/// ECMAScript `Number::toString` (ECMA-262 7.1.12.1 / RFC 8785 §3.2.2.3) for a
/// finite f64.
///
/// `ryu` gives the SHORTEST decimal that round-trips to `f`, but as a *string*
/// (e.g. `"1.2e21"`, `"0.000001"`). [`parse_shortest_decimal`] turns that string
/// back into the bare significant digits `s` (length `k`, no leading/trailing
/// insignificant zeros) and a decimal point position `n` such that the value is
/// `0.s * 10^n` — i.e. the first significant digit sits at place value
/// `10^(n-1)`. Then we re-emit per the four ES6 branches:
///
/// * `k <= n <= 21`  -> the `k` digits then `n-k` trailing zeros, no `.`
/// * `0 < n <= 21`   -> first `n` digits, `.`, remaining `k-n` digits
/// * `-6 < n <= 0`   -> `0.`, `-n` zeros, then the `k` digits
/// * `n > 21 || n <= -6` -> exponential: `s[0]`, optional `.`+`s[1..]`,
///   `e`, sign, `|n-1|`
///
/// `0.0` and `-0.0` both canonicalize to `"0"`.
fn canonical_jcs_float(f: f64) -> Vec<u8> {
    debug_assert!(f.is_finite(), "non-finite reaches canonical_jcs_float");

    if f == 0.0 {
        // Covers both +0.0 and -0.0 (RFC 8785: negative zero serializes as "0").
        return b"0".to_vec();
    }

    let negative = f < 0.0;

    // ryu emits the shortest round-tripping decimal for |f|.
    let mut ryu_buf = ryu::Buffer::new();
    let shortest = ryu_buf.format_finite(f.abs());

    // Parse ryu's string into (digits `s`, decimal position `n`).
    let (digits, n) = parse_shortest_decimal(shortest);

    // `digits` is the significand with no leading or trailing insignificant
    // zeros; `n` is the position of the decimal point relative to the start of
    // `digits` (value = 0.<digits> * 10^n).
    let k = digits.len() as i64;
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }

    if n >= k && n <= 21 {
        // Integer with trailing zeros, no decimal point.
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        // Split inside the digit run.
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        // Leading "0." then -n zeros then all digits.
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        // Exponential. First digit, then optional fractional digits, then exponent.
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let exp = n - 1;
        if exp >= 0 {
            out.push('+');
            out.push_str(&exp.to_string());
        } else {
            out.push('-');
            out.push_str(&(-exp).to_string());
        }
    }

    out.into_bytes()
}

/// Parse a ryu shortest-float string for a positive, finite, non-zero magnitude
/// into `(digits, n)` where `digits` holds only the significant digits (no
/// leading/trailing insignificant zeros, no sign, no point) and `n` is the
/// decimal point position such that value = `0.<digits> * 10^n`.
///
/// ryu formats are one of: `"DDD"`, `"DDD.DDD"`, `"D.DDDeNN"`, `"DeNN"`,
/// `"0.000DDD"`, etc. (lowercase `e`, the sign already stripped by the caller
/// via `f.abs()`).
fn parse_shortest_decimal(s: &str) -> (String, i64) {
    // Split optional exponent.
    let (mantissa, exp10) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (
            m,
            e.parse::<i64>().expect("ryu exponent is a valid integer"),
        ),
        None => (s, 0),
    };

    // Split mantissa into integer / fractional digit runs around the point.
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };

    // Concatenate all digits. The original decimal point sits after int_part, so
    // value = <int_part frac_part> * 10^(exp10 - frac_len). Expressed in the
    // 0.<digits>*10^n convention, the point n (before trimming) is at:
    let mut all: String = String::with_capacity(int_part.len() + frac_part.len());
    all.push_str(int_part);
    all.push_str(frac_part);
    let mut point: i64 = int_part.len() as i64 + exp10;

    // Trim leading zeros (each shifts the point left by one).
    let leading_zeros = all.len() - all.trim_start_matches('0').len();
    if leading_zeros > 0 {
        all.drain(..leading_zeros);
        point -= leading_zeros as i64;
    }

    // Trim trailing zeros (they do not affect the point in 0.<digits>*10^n form).
    let trimmed_len = all.trim_end_matches('0').len();
    all.truncate(trimmed_len);

    // `all` is now the bare significant digits; `point` is n.
    (all, point)
}

/// Detect duplicate keys WITHIN THE SAME OBJECT in raw JSON text, per RFC 8785
/// §3.2.1. Used by the strict path to reject untrusted wire JSON before signing.
///
/// Object-scoped: maintains a stack of brace-depth frames, one `HashSet<String>`
/// per open object. `{"a":1,"b":{"a":2}}` is ACCEPTED (same name, different
/// objects); `{"a":1,"a":2}` and `{"x":{"a":1,"a":2}}` are REJECTED. String and
/// escape context is tracked so a `{`, `}`, `"`, or `:` inside a string value is
/// never miscounted, and only strings in *key position* (immediately followed by
/// `:`) are recorded.
fn has_duplicate_keys(s: &str) -> bool {
    // One HashSet per currently-open object; arrays push no frame (no keys).
    let mut object_frames: Vec<std::collections::HashSet<String>> = Vec::new();
    let bytes = s.as_bytes();
    let mut pos = 0usize;

    while pos < bytes.len() {
        match bytes[pos] {
            b'{' => {
                object_frames.push(std::collections::HashSet::new());
                pos += 1;
            }
            b'}' => {
                object_frames.pop();
                pos += 1;
            }
            b'"' => {
                // Scan the full string token (with escape awareness).
                let str_start = pos + 1;
                let mut i = str_start;
                let mut esc = false;
                while i < bytes.len() {
                    let c = bytes[i];
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    // Unterminated string; let serde_json::from_str report the
                    // real parse error downstream.
                    break;
                }
                let token = &bytes[str_start..i];
                // Is this string in KEY position? (next non-ws byte is ':')
                let mut look = i + 1;
                while look < bytes.len() && matches!(bytes[look], b' ' | b'\t' | b'\n' | b'\r') {
                    look += 1;
                }
                let is_key = look < bytes.len() && bytes[look] == b':';
                if is_key && let Some(frame) = object_frames.last_mut() {
                    // Compare DECODED key names, not raw token bytes: RFC 8785
                    // §3.2.1 forbids duplicate keys by their member-name VALUE, so
                    // `{"a":1,"a":2}` is a duplicate even though the raw
                    // tokens differ. Decode JSON string escapes by reparsing the
                    // token as a JSON string. A token that fails to decode is left
                    // for `serde_json::from_str` to report as a parse error
                    // downstream; we conservatively skip recording it (the strict
                    // path still errors on the malformed input before signing).
                    if let Some(key) = decode_json_string_token(token)
                        && !frame.insert(key)
                    {
                        return true; // dup within this same object
                    }
                }
                pos = i + 1;
            }
            _ => {
                pos += 1;
            }
        }
    }
    false
}

/// Decode a raw JSON string token (the bytes BETWEEN the surrounding quotes, with
/// escapes still encoded) into its actual member-name value. Returns `None` if the
/// token is not valid UTF-8 or not a well-formed JSON string body (left for the
/// downstream strict parse to report). Used by [`has_duplicate_keys`] so the
/// duplicate check compares decoded names per RFC 8785 §3.2.1 (`{"a":1,"a":2}`
/// is a duplicate). `serde_json` is already a runtime dep; no new dependency.
fn decode_json_string_token(token: &[u8]) -> Option<String> {
    let inner = std::str::from_utf8(token).ok()?;
    // Re-wrap in quotes and parse as a JSON string to apply standard escape
    // decoding (\uXXXX incl. surrogate pairs, \n, \", \\, etc.).
    let mut quoted = String::with_capacity(inner.len() + 2);
    quoted.push('"');
    quoted.push_str(inner);
    quoted.push('"');
    serde_json::from_str::<String>(&quoted).ok()
}

// --- internal: finite-check serializer (non-finite guard at the typed boundary) ---

mod finite_check;
use finite_check::check_finite;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jcs(v: &serde_json::Value) -> String {
        String::from_utf8(to_jcs_bytes(v).unwrap()).unwrap()
    }

    #[test]
    fn test_jcs_canonical_sorting() {
        let unordered = json!({ "z": 1, "a": 2, "m": 3 });
        assert_eq!(jcs(&unordered), r#"{"a":2,"m":3,"z":1}"#);
    }

    // --- key ordering: UTF-16 code units, NOT Unicode-scalar (W5.1 FIX 1) ---

    #[test]
    fn test_key_sort_utf16_not_scalar_e000_vs_10000() {
        // The T-Rex case. Scalar/UTF-8 order: U+E000 (0xE000) < U+10000 (0x10000),
        // so a scalar sort would emit E000 first. RFC 8785 §3.2.3 uses UTF-16 code
        // units: U+10000 is the surrogate pair (0xD800, 0xDC00), and 0xD800 < 0xE000,
        // so the conformant order emits U+10000 FIRST. This pins the bug fix.
        let obj = json!({ "\u{E000}": 1, "\u{10000}": 2 });
        let s = jcs(&obj);
        let e000_pos = s.find('\u{E000}').expect("E000 key present");
        let high_pos = s.find('\u{10000}').expect("10000 key present");
        assert!(
            high_pos < e000_pos,
            "RFC 8785 UTF-16 order must emit U+10000 before U+E000; got {s}"
        );
        // Exact bytes (U+10000 = f0 90 80 80, U+E000 = ee 80 80).
        assert_eq!(s, "{\"\u{10000}\":2,\"\u{E000}\":1}");
    }

    #[test]
    fn test_key_sort_surrogate_boundary_mixed() {
        // Mixed ASCII / BMP / non-BMP keys must come out in UTF-16 order:
        // "A"(0x41) < "\u{E000}"(0xE000) < "\u{10000}"(D800 DC00? no — D800 < E000).
        // UTF-16 ranking: A (0x0041) < 𐀀 U+10000 (0xD800..) < emoji U+1F600 (0xD83D..)
        //   < private-use U+E000 (0xE000) < U+FFFF? Careful: 0xD800 < 0xD83D < 0xE000.
        let obj = json!({
            "\u{E000}": 1,    // BMP private-use, single u16 0xE000
            "\u{1F600}": 2,   // emoji, surrogate pair starting 0xD83D
            "A": 3,           // ASCII 0x41
            "\u{10000}": 4,   // surrogate pair starting 0xD800
        });
        let s = jcs(&obj);
        let pa = s.find('A').unwrap();
        let p10000 = s.find('\u{10000}').unwrap();
        let pemoji = s.find('\u{1F600}').unwrap();
        let pe000 = s.find('\u{E000}').unwrap();
        // UTF-16 order: A(0x41) < U+10000(0xD800) < U+1F600(0xD83D) < U+E000(0xE000)
        assert!(
            pa < p10000 && p10000 < pemoji && pemoji < pe000,
            "UTF-16 key order violated: A={pa} 10000={p10000} 1F600={pemoji} E000={pe000} in {s}"
        );
    }

    #[test]
    fn test_integer_fast_paths() {
        assert_eq!(jcs(&json!(0)), "0");
        assert_eq!(jcs(&json!(-1)), "-1");
        assert_eq!(jcs(&json!(9007199254740992i64)), "9007199254740992");
        assert_eq!(jcs(&json!(u64::MAX)), "18446744073709551615");
        assert_eq!(jcs(&json!(i64::MIN)), "-9223372036854775808");
    }

    #[test]
    fn test_float_zero_and_integer_form() {
        assert_eq!(jcs(&json!(-0.0)), "0");
        assert_eq!(jcs(&json!(0.0)), "0");
        // Whole-valued floats render without a trailing ".0".
        assert_eq!(jcs(&json!(10.0)), "10");
        assert_eq!(jcs(&json!(1.0)), "1");
        assert_eq!(jcs(&json!(-1.0)), "-1");
    }

    #[test]
    fn test_es6_exponential_cutovers() {
        // n>21 cutover: 1e21 is the first exponential on the high side.
        assert_eq!(jcs(&json!(1e21)), "1e+21");
        // 1e20 is still fixed (21 digits, no point).
        assert_eq!(jcs(&json!(1e20)), "100000000000000000000");
        // PRE-W5 BUG: 1e10 was wrongly scientific; ES6 keeps it fixed.
        assert_eq!(jcs(&json!(1e10)), "10000000000");
        // n<=-6 cutover: 1e-7 is exponential, 1e-6 is fixed.
        assert_eq!(jcs(&json!(1e-7)), "1e-7");
        assert_eq!(jcs(&json!(1e-6)), "0.000001");
    }

    #[test]
    fn test_es6_precision_shortest_roundtrip() {
        // 17-significant-digit shortest round-trip forms.
        assert_eq!(jcs(&json!(0.1)), "0.1");
        assert_eq!(jcs(&json!(0.3333333333333333f64)), "0.3333333333333333");
        assert_eq!(jcs(&json!(f64::MAX)), "1.7976931348623157e+308");
        assert_eq!(jcs(&json!(5e-324f64)), "5e-324"); // smallest subnormal
        assert_eq!(jcs(&json!(-5e-324f64)), "-5e-324");
    }

    #[test]
    fn test_round_trip_parses_back() {
        for x in [
            0.1f64,
            1e-7,
            1e21,
            42.7654,
            -2.5,
            1.7976931348623157e308,
            5e-324,
            123456.789,
            0.00525,
        ] {
            let s = String::from_utf8(canonical_jcs_float(x)).unwrap();
            let back: f64 = s.parse().unwrap();
            assert_eq!(back, x, "round-trip failed for {x}: got {s}");
        }
    }

    // --- dup-key detector: object-scoped (W5 Q4) ---

    #[test]
    fn test_dup_same_object_rejected() {
        assert!(has_duplicate_keys(r#"{"a":1,"a":2}"#));
        assert!(to_jcs_bytes_strict(r#"{"a":1,"a":2}"#).is_err());
    }

    #[test]
    fn test_dup_nested_object_rejected() {
        assert!(has_duplicate_keys(r#"{"x":{"a":1,"a":2}}"#));
        assert!(to_jcs_bytes_strict(r#"{"x":{"a":1,"a":2}}"#).is_err());
    }

    #[test]
    fn test_same_name_different_objects_accepted() {
        // Legal per RFC 8785 §3.2.1 — same name, different objects.
        assert!(!has_duplicate_keys(r#"{"a":1,"b":{"a":2}}"#));
        to_jcs_bytes_strict(r#"{"a":1,"b":{"a":2}}"#).unwrap();
        // Sibling objects in an array each get a fresh frame.
        assert!(!has_duplicate_keys(r#"[{"a":1},{"a":2}]"#));
        // Pre-W5 BUG case: the global name counter false-rejected this.
        assert!(!has_duplicate_keys(r#"{"a":1,"b":{"a":2},"c":{"a":3}}"#));
    }

    #[test]
    fn test_dup_escaped_spelling_rejected() {
        // RFC 8785 §3.2.1 compares DECODED member names: `a` IS `a`, so
        // `{"a":1,"a":2}` is a duplicate even though the raw tokens differ.
        // Pre-W5.1 the raw-token compare missed this and `serde_json::from_str`
        // then silently collapsed them into one signed object.
        assert!(has_duplicate_keys(r#"{"a":1,"a":2}"#));
        assert!(to_jcs_bytes_strict(r#"{"a":1,"a":2}"#).is_err());
        // Both keys ESCAPED (\u0061 == \u0061), same decoded name -> dup.
        // (Was a copy/paste of the plain-literal case above and inert; now a
        // real escaped-vs-escaped collision so the decode path is exercised.)
        assert!(has_duplicate_keys("{\"\\u0061\":1,\"\\u0061\":2}"));
        assert!(to_jcs_bytes_strict("{\"\\u0061\":1,\"\\u0061\":2}").is_err());
        // Escaped spelling colliding with the PLAIN literal `a`.
        assert!(has_duplicate_keys("{\"\\u0061\":1,\"a\":2}"));
        // Escaped newline-bearing key colliding with its literal form.
        assert!(has_duplicate_keys("{\"a\\nb\":1,\"a\\u000ab\":2}"));
        // Distinct decoded names with escapes must NOT be flagged.
        assert!(!has_duplicate_keys(r#"{"a":1,"b":2}"#));
        to_jcs_bytes_strict(r#"{"a":1,"b":2}"#).unwrap();
        // Surrogate-pair escape (😀 = U+1F600) vs its literal form: dup.
        assert!(has_duplicate_keys("{\"\\uD83D\\uDE00\":1,\"\u{1F600}\":2}"));
    }

    #[test]
    fn test_key_like_substring_in_value_not_counted() {
        // "a" appears inside a string VALUE, not as a second key.
        assert!(!has_duplicate_keys(r#"{"a":"a","b":"\"a\":zzz"}"#));
        // Braces/colons inside a string value must not open frames or count keys.
        assert!(!has_duplicate_keys(r#"{"a":"{\"a\":1,\"a\":2}"}"#));
        // Escaped quote inside value then a real second distinct key.
        assert!(!has_duplicate_keys(r#"{"k1":"v\"x","k2":"y"}"#));
    }

    // --- non-finite guard (W5 P0, defense-in-depth at the typed boundary) ---

    #[test]
    fn test_nonfinite_struct_field_rejected() {
        #[derive(serde::Serialize)]
        struct Money {
            cost_usd: f64,
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let r = to_jcs_bytes(&Money { cost_usd: bad });
            assert!(
                matches!(r, Err(JcsError::NonFinite)),
                "non-finite {bad} must be rejected, got {r:?}"
            );
        }
    }

    #[test]
    fn test_finite_extremes_accepted() {
        #[derive(serde::Serialize)]
        struct Money {
            cost_usd: f64,
        }
        for ok in [5e-324f64, f64::MAX, f64::MIN, 0.0, -0.0, 0.0079, 0.125] {
            assert!(
                to_jcs_bytes(&Money { cost_usd: ok }).is_ok(),
                "finite {ok} must be accepted"
            );
        }
    }

    #[test]
    fn test_nonfinite_bare_f64_rejected() {
        assert!(matches!(to_jcs_bytes(&f64::NAN), Err(JcsError::NonFinite)));
    }

    #[test]
    fn test_to_value_silently_nulls_nonfinite_guard_must_be_on_typed_t() {
        // Pins the FIX-3 probe result that justifies keeping check_finite on the
        // TYPED T (not on the post-to_value Value): serde_json::to_value collapses
        // a NaN/Inf struct field to JSON null WITHOUT error. If a future refactor
        // moves the finite check to run on the Value, the guard would silently
        // regress — this asserts the collapse so the danger stays documented.
        #[derive(serde::Serialize)]
        struct Money {
            cost_usd: f64,
        }
        let v = serde_json::to_value(Money { cost_usd: f64::NAN }).unwrap();
        assert_eq!(
            v,
            json!({ "cost_usd": null }),
            "to_value nulls NaN silently"
        );
        // And the actual guard (on T) still catches it before any bytes.
        assert!(matches!(
            to_jcs_bytes(&Money { cost_usd: f64::NAN }),
            Err(JcsError::NonFinite)
        ));
    }

    // --- single-pass purity trap (W5.1 P1-C: make the trap permanent) ---

    /// A deliberately NON-PURE `Serialize`: it emits a different value on every
    /// pass (an interior-mutable counter). This is exactly the hand-rolled,
    /// stateful impl the W5.1 single-pass requirement forbids — signed types MUST
    /// derive `Serialize`, never hand-roll one whose tree changes across passes.
    struct NonDeterministic {
        counter: std::cell::Cell<i64>,
    }

    impl serde::Serialize for NonDeterministic {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let v = self.counter.get();
            self.counter.set(v + 1);
            s.serialize_i64(v)
        }
    }

    // debug_assert! is compiled out under `cargo test --release`; this test
    // asserts the trap FIRES, so it is only valid where debug-assertions are on.
    #[cfg(debug_assertions)]
    #[test]
    fn test_single_pass_purity_trap_fires_on_nondeterministic_serialize() {
        // The `to_jcs_bytes` debug_assert re-serializes T and asserts the second
        // tree equals the first. A non-pure impl makes the two trees differ, so
        // the assert MUST fire (panic) in test/debug builds. This pins the trap so
        // it can never be silently removed. (debug_assert is compiled in test
        // builds; zero cost in release.)
        let value = NonDeterministic {
            counter: std::cell::Cell::new(0),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = to_jcs_bytes(&value);
        }));
        assert!(
            result.is_err(),
            "the single-pass determinism debug_assert must fire on a non-pure Serialize impl"
        );
    }

    // --- struct key sort (regression) ---

    #[test]
    fn test_jcs_struct_key_sort() {
        #[derive(serde::Serialize)]
        struct S {
            z: i32,
            a: i32,
            m: String,
        }
        let bytes = to_jcs_bytes(&S {
            z: 1,
            a: 2,
            m: "x".into(),
        })
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"a":2,"m":"x","z":1}"#
        );
    }
}
