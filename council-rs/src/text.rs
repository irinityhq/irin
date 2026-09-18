//! Small text utilities shared across crate layers.
//!
//! Kept in a leaf module so provider transports and warroom helpers do not
//! depend on the deliberation engine for a string operation.

/// Byte-bounded prefix that never splits a UTF-8 character. Every
/// user- or provider-derived truncation goes through here (B-08).
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::truncate_utf8;

    /// B-08: a cut that lands inside a multi-byte character must back off to
    /// the previous boundary instead of panicking (`&prompt[..3000]` did).
    #[test]
    fn truncate_utf8_backs_off_from_mid_character_cut() {
        // 2999 ASCII bytes, then a 3-byte character straddling byte 3000.
        let prompt = format!("{}€tail", "a".repeat(2999));
        assert_eq!(prompt.len(), 2999 + 3 + 4);
        assert!(!prompt.is_char_boundary(3000));
        let cut = truncate_utf8(&prompt, 3000);
        assert_eq!(cut.len(), 2999);
        assert!(cut.is_char_boundary(cut.len()));
        assert_eq!(truncate_utf8("短い", 100), "短い");
        assert_eq!(truncate_utf8("日本語", 4), "日");
    }
}
