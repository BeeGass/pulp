//! Fast, tokenizer-free size estimate for dump summaries.

/// Approximate LLM token count with a single linear scan.
///
/// CJK ideographs, kana, and hangul count as one token per character. Other
/// non-whitespace characters count as 1/4 token (four characters per token).
/// Whitespace is skipped. The result is rounded up.
#[must_use]
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        if is_cjk_kana_hangul(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk.saturating_add(other.div_ceil(4))
}

fn is_cjk_kana_hangul(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF66}'..='\u{FF9D}'
            | '\u{20000}'..='\u{2CEAF}'
            | '\u{2F800}'..='\u{2FA1F}'
            | '\u{30000}'..='\u{323AF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_with_short_ascii_returns_ceil_quarter() {
        // 5 non-whitespace chars -> ceil(5/4) = 2
        assert_eq!(estimate_tokens("hello"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
    }
}
