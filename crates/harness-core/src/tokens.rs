//! Rough token counts. harness has no tokenizer for most models, so it counts about four bytes
//! per token, which is close for English text and code.

/// The context window assumed for every model until model profiles report the real one.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

/// Estimated tokens in `text`: its length in bytes divided by four, rounded up.
pub fn estimate(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_bytes_make_a_token() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate(&"x".repeat(12_000)), 3_000);
    }
}
