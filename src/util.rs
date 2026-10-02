// ============================================================================
// SearchWala v8.0 — Shared utilities
//
// UTF-8-safe string handling. The v7.0 codebase byte-sliced `&s[..n]` in four
// places, which panics whenever the cut point lands mid-character. A Hindi
// query over ~40 characters with any ASCII byte before the cut aborted the
// request task (there was no CatchPanicLayer, so the client got a connection
// reset with no response).
//
// Repro that motivated this module:
//   "5" + "क"*80  (241 bytes) -> &s[..120] -> PANIC
// ============================================================================

/// Truncate to at most `max_chars` **characters** without splitting a UTF-8
/// character. Returns a borrowed slice — no allocation in the common path.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Truncate to at most `max_bytes` bytes, backing up to the nearest character
/// boundary. Use when the limit is a real byte budget (buffers, wire formats)
/// rather than a display length.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Case-insensitive substring search returning a **valid byte offset into
/// `hay`**.
///
/// `hay.to_lowercase().find(..)` is unsafe to use for indexing `hay`:
/// lowercasing can change byte length (`İ` U+0130 -> 2 chars, Greek final
/// sigma, Turkish dotted I), so offsets from the lowercased copy may be out of
/// bounds or off-boundary on the original.
pub fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    find_ci_from(hay, needle, 0)
}

/// As [`find_ci`], but only considers matches starting at or after `from`.
pub fn find_ci_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle_lower.is_empty() {
        return None;
    }

    for (idx, _) in hay.char_indices() {
        if idx < from {
            continue;
        }
        let mut hay_chars = hay[idx..].chars().flat_map(char::to_lowercase);
        let mut matched = true;
        for want in &needle_lower {
            match hay_chars.next() {
                Some(got) if got == *want => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            return Some(idx);
        }
    }
    None
}

/// Character count. Named to make byte-vs-char intent explicit at call sites —
/// v7.0 used `text.len()` (bytes) and `text.chars().count()` interchangeably
/// for the same `char_count` field, which inflated Devanagari/CJK scores ~3x.
#[inline]
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// True for characters that belong inside a word.
///
/// `char::is_alphanumeric` excludes combining marks such as the Devanagari
/// virama (्), so splitting on it broke every Indic conjunct: "क्या" became
/// ["क", "या"] and "प्रधानमंत्री" three fragments. Query terms, BM25 tokens
/// and citation checks all tokenized Hindi that way. Dandas (। ॥) remain
/// separators; ZWJ/ZWNJ, which shape conjuncts, stay inside the word.
#[inline]
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || (('\u{0900}'..='\u{0DFF}').contains(&c) && !matches!(c, '\u{0964}' | '\u{0965}'))
        || matches!(c, '\u{200C}' | '\u{200D}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indic_conjuncts_stay_whole_words() {
        let split = |s: &str| -> Vec<String> {
            s.split(|c: char| !is_word_char(c)).filter(|t| !t.is_empty()).map(str::to_string).collect()
        };
        assert_eq!(split("क्या है"), vec!["क्या", "है"]);
        assert_eq!(split("प्रधानमंत्री।"), vec!["प्रधानमंत्री"]);
        assert_eq!(split("हिन्दी, தமிழ்"), vec!["हिन्दी", "தமிழ்"]);
        assert_eq!(split("rust-lang 2.0"), vec!["rust", "lang", "2", "0"]);
    }

    #[test]
    fn truncate_chars_never_panics_on_devanagari() {
        // The exact inputs that panicked in v7.0 at search.rs:143.
        let cases = [
            "क".repeat(80),
            format!("5{}", "क".repeat(80)),
            format!("IP{}", "क".repeat(80)),
            "🔥".repeat(40),
            format!("{{{}", "क".repeat(200)),
            "मुझे भारत और पाकिस्तान के बीच विश्व कप मुकाबले की पूरी जानकारी चाहिए".to_string(),
        ];
        for c in &cases {
            let out = truncate_chars(c, 120);
            assert!(out.chars().count() <= 120);
            assert!(c.starts_with(out), "truncation must be a prefix");
        }
    }

    #[test]
    fn truncate_chars_is_noop_when_short() {
        assert_eq!(truncate_chars("hello", 120), "hello");
        assert_eq!(truncate_chars("", 10), "");
    }

    #[test]
    fn truncate_bytes_backs_up_to_boundary() {
        let s = format!("5{}", "क".repeat(80)); // 1 + 240 bytes
        let out = truncate_bytes(&s, 120);
        assert!(out.len() <= 120);
        assert!(s.starts_with(out));
        // 120 is mid-character here, so it must have backed up.
        assert!(out.len() < 120);
    }

    #[test]
    fn find_ci_handles_length_changing_lowercase() {
        // 'İ' lowercases to two chars — offsets from a lowercased copy would
        // be wrong, but find_ci returns an offset into the ORIGINAL.
        let hay = "aİb<THINK>x</THINK>";
        let idx = find_ci(hay, "<think>").expect("should find");
        assert!(hay.is_char_boundary(idx));
        assert_eq!(&hay[idx..idx + 7], "<THINK>");
    }

    #[test]
    fn find_ci_from_respects_start_offset() {
        let hay = "<think>a</think><think>b</think>";
        let first = find_ci(hay, "<think>").unwrap();
        let close = find_ci_from(hay, "</think>", first + 7).unwrap();
        // The close tag found must come AFTER the open tag, which the v7.0
        // post_process_answer did not guarantee (it duplicated text).
        assert!(close > first);
    }

    #[test]
    fn find_ci_returns_none_when_absent() {
        assert_eq!(find_ci("hello world", "<think>"), None);
        assert_eq!(find_ci("", "x"), None);
        assert_eq!(find_ci("abc", ""), None);
    }
}
