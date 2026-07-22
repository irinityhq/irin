//! First-user-message → conversation title (pure function).
//!

use std::sync::OnceLock;

use regex::Regex;

pub const FALLBACK: &str = "Untitled conversation";
pub const MAX_CHARS: usize = 50;

fn ws_run() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+").unwrap())
}

pub fn from_first_message(content: &str) -> String {
    if content.is_empty() {
        return FALLBACK.to_string();
    }
    let flat = ws_run().replace_all(content, " ");
    let trimmed = flat.trim();
    if trimmed.is_empty() {
        return FALLBACK.to_string();
    }
    trimmed.chars().take(MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_fallback() {
        assert_eq!(from_first_message(""), FALLBACK);
        assert_eq!(from_first_message("   \n\t  "), FALLBACK);
    }

    #[test]
    fn collapses_whitespace_and_trims() {
        assert_eq!(
            from_first_message("hello   world\n  again"),
            "hello world again"
        );
    }

    #[test]
    fn truncates_to_50_chars() {
        let s = "x".repeat(80);
        assert_eq!(from_first_message(&s).chars().count(), 50);
    }
}
