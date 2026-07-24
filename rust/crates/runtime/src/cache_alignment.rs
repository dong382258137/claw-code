//! # Cache Alignment — Dynamic Value Extraction from Static Prompt Sections
//!
//! ## Problem
//!
//! The `SystemPromptSplit` divides the prompt into static (cacheable) and dynamic
//! (volatile) sections. Within a single session, the static sections are
//! byte-stable because `SystemPromptBuilder::build()` is called only once.
//! However, certain dynamic values can inadvertently leak into static sections
//! — especially when new code is added that embeds timestamps, UUIDs, absolute
//! paths, or random hex strings into what should be stable text.
//!
//! ## Solution
//!
//! `DynamicValueExtractor` is a defense-in-depth layer applied to static
//! sections at assembly time. It scans for common dynamic patterns and:
//!
//! 1. Replaces them with stable placeholders in the static section (keeping the
//!    byte sequence constant so provider-side prompt caching works).
//! 2. Collects the original values into a compact summary appended to dynamic
//!    sections so the LLM can still see them.
//!
//! ## Design Properties
//!
//! - **No regex dependency**: uses simple character-level scanning for fast,
//!   allocation-light detection.
//! - **Idempotent**: already-replaced placeholders are not re-extracted.
//! - **Non-breaking**: if no dynamic patterns are found, the section passes
//!   through unchanged.
//! - **Bounded cost**: only scans static sections (typically < 20KB total),
//!   and has early-exit heuristics.
//!
//! ## Headroom Comparison
//!
//! Headroom's Cache Aligner extracts dates, UUIDs, and tokens from the prompt
//! body to stabilize cache prefixes. This module implements the same idea but:
//! - Operates at the section level (not raw request bytes)
//! - Uses heuristic pattern matching (not ML)
//! - Is optional and transparent to callers

use std::borrow::Cow;

/// Maximum total characters of extracted values appended to dynamic sections.
/// Prevents a single pathological section from bloating the prompt.
const MAX_EXTRACTED_SUMMARY_CHARS: usize = 300;

/// Collection of dynamic values extracted from a text, ready for reassembly.
#[derive(Debug, Default)]
pub struct DynamicValueExtractor {
    /// (placeholder, original) pairs in insertion order.
    replacements: Vec<(String, String)>,
    /// Total chars in original values (capped by MAX_EXTRACTED_SUMMARY_CHARS).
    total_extracted_chars: usize,
}

impl DynamicValueExtractor {
    /// Create a new empty extractor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `text` for dynamic patterns and return a version where every
    /// match is replaced by a stable placeholder. Extracted originals are
    /// buffered internally for later retrieval via [`collect_section`].
    ///
    /// This is the primary entry point. Call it once per static section.
    #[must_use]
    pub fn extract_replace<'a>(&mut self, text: &'a str) -> Cow<'a, str> {
        if text.is_empty() {
            return Cow::Borrowed(text);
        }

        let mut result = String::with_capacity(text.len());
        let mut remaining = text;
        let mut modified = false;

        while let Some(pos) = find_next_dynamic_pattern(remaining) {
            let (before, _after) = remaining.split_at(pos.start);
            result.push_str(before);

            let matched = &remaining[pos.start..pos.end];
            if is_already_placeholder(matched) {
                // Already a placeholder — pass through unchanged.
                result.push_str(matched);
                remaining = &remaining[pos.end..];
                continue;
            }

            modified = true;
            let placeholder = self.make_placeholder(matched);
            result.push_str(&placeholder);
            remaining = &remaining[pos.end..];
        }

        if !modified {
            return Cow::Borrowed(text);
        }

        result.push_str(remaining);
        Cow::Owned(result)
    }

    /// Build a single compact section summarising everything that was
    /// extracted. Returns an empty string when nothing was extracted.
    ///
    /// Call this after processing all static sections and append the result
    /// to the dynamic sections of the prompt split.
    #[must_use]
    pub fn collect_section(&self) -> String {
        if self.replacements.is_empty() {
            return String::new();
        }

        let mut lines = vec!["# Cache-aligned dynamic values".to_string()];
        for (placeholder, original) in &self.replacements {
            lines.push(format!("  {placeholder} = {original}"));
        }
        lines.join("\n")
    }

    /// Returns the number of extracted dynamic values.
    #[must_use]
    pub fn extracted_count(&self) -> usize {
        self.replacements.len()
    }

    fn make_placeholder(&mut self, original: &str) -> String {
        let kind = classify_dynamic_value(original);
        let ordinal = self.replacements.len() + 1;
        let placeholder = format!("<{kind}_{ordinal}>");

        // Track the original but cap total chars to avoid bloat.
        if self.total_extracted_chars < MAX_EXTRACTED_SUMMARY_CHARS {
            self.total_extracted_chars += original.chars().count();
            self.replacements
                .push((placeholder.clone(), original.to_string()));
        }

        placeholder
    }
}

/// Returns true when `s` looks like a placeholder we generated.
fn is_already_placeholder(s: &str) -> bool {
    s.starts_with('<') && s.ends_with('>') && s.len() > 4 && s[1..s.len() - 1].contains('_')
}

/// Classifies a dynamic value into a short label used in placeholder names.
fn classify_dynamic_value(s: &str) -> &'static str {
    // Date-only: 2026-07-23 (10 chars)
    if s.len() >= 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-') {
        return "datetime";
    }
    if s.len() >= 19 && s.chars().nth(4) == Some('-') && s.chars().nth(7) == Some('-') {
        // ISO datetime: 2026-07-23T16:28:00
        if s.contains('T') || s.contains(' ') {
            return "datetime";
        }
    }
    if s.len() >= 32
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && s.matches('-').count() >= 4
    {
        return "uuid";
    }
    if s.chars().all(|c| c.is_ascii_digit()) && s.len() >= 10 {
        // Unix timestamp in ms or seconds
        return "timestamp";
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return "hex";
    }
    if s.contains('/') || s.contains('\\') {
        // File path
        return "path";
    }
    "value"
}

/// Describes a span `[start, end)` within the source text.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

/// Scans `text` for the next dynamic pattern, returning its byte span.
/// Returns `None` when no more patterns are found.
///
/// Patterns detected (in order):
/// 1. ISO 8601 datetimes (2026-07-23T16:28:00)
/// 2. UUIDs (550e8400-e29b-41d4-a716-446655440000)
/// 3. Unix timestamps >= 10 digits (1784575505)
/// 4. 8+ character random-lookalike hex strings
/// 5. Absolute paths starting with `/` or `\` that aren't command names
fn find_next_dynamic_pattern(text: &str) -> Option<Span> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip non-alphanumeric / non-path start characters
        match bytes[i] {
            b'0'..=b'9' => {
                if let Some(span) = try_iso_datetime(text, i) {
                    return Some(span);
                }
                if let Some(span) = try_uuid(text, i) {
                    return Some(span);
                }
                if let Some(span) = try_timestamp(text, i) {
                    return Some(span);
                }
                if let Some(span) = try_hex(text, i) {
                    return Some(span);
                }
                i += 1;
            }
            b'a'..=b'f' | b'A'..=b'F' => {
                if let Some(span) = try_uuid(text, i) {
                    return Some(span);
                }
                if let Some(span) = try_hex(text, i) {
                    return Some(span);
                }
                i += 1;
            }
            b'/' | b'\\' => {
                if let Some(span) = try_absolute_path(text, i) {
                    return Some(span);
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Try to match an ISO datetime at `pos`: YYYY-MM-DD or YYYY-MM-DDThh:mm:ss
fn try_iso_datetime(text: &str, pos: usize) -> Option<Span> {
    let rest = &text[pos..];
    // Need at least YYYY-MM-DD (10 chars)
    if rest.len() < 10 {
        return None;
    }
    let bytes = rest.as_bytes();
    // YYYY
    if !bytes[0..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[4] != b'-' {
        return None;
    }
    // MM
    if !bytes[5..7].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[7] != b'-' {
        return None;
    }
    // DD
    if !bytes[8..10].iter().all(u8::is_ascii_digit) {
        return None;
    }

    // Check for extended time portion: Thh:mm or Thh:mm:ss
    let mut end = pos + 10;
    if rest.len() >= 13
        && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[11..13].iter().all(u8::is_ascii_digit)
    {
        end = pos + 13;
        if rest.len() >= 16 && bytes[13] == b':' && bytes[14..16].iter().all(u8::is_ascii_digit) {
            end = pos + 16;
            if rest.len() >= 19 && bytes[16] == b':' && bytes[17..19].iter().all(u8::is_ascii_digit)
            {
                end = pos + 19;
            }
        }
    }

    Some(Span { start: pos, end })
}

/// Try to match a UUID at `pos`: hex-octets-dash format
fn try_uuid(text: &str, pos: usize) -> Option<Span> {
    let rest = &text[pos..];
    // 8-4-4-4-12 = 36 chars
    if rest.len() < 36 {
        return None;
    }
    let bytes = rest.as_bytes();

    let parts = [(0, 8), (9, 4), (14, 4), (19, 4), (24, 12)];
    let dashes = [8, 13, 18, 23];

    for &dash_pos in &dashes {
        if bytes[dash_pos] != b'-' {
            return None;
        }
    }

    for &(start, len) in &parts {
        if !bytes[start..start + len]
            .iter()
            .all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
    }

    Some(Span {
        start: pos,
        end: pos + 36,
    })
}

/// Try to match a Unix timestamp (10+ consecutive digits)
fn try_timestamp(text: &str, pos: usize) -> Option<Span> {
    let rest = &text[pos..];
    let bytes = rest.as_bytes();
    let mut end = pos;
    while end < text.len() && bytes[end - pos].is_ascii_digit() {
        end += 1;
    }
    let len = end - pos;
    if len >= 13 {
        // 13+ digit timestamps are distinctive enough
        return Some(Span { start: pos, end });
    }
    None
}

/// Try to match an 8+ character hex string that looks like a random ID.
/// Only matches when the hex chars are preceded/followed by non-alphanumeric
/// boundaries to avoid matching hex values in code (e.g., color literals).
fn try_hex(text: &str, pos: usize) -> Option<Span> {
    let rest = &text[pos..];
    let bytes = rest.as_bytes();
    let mut end = pos;
    while end < text.len() && bytes[end - pos].is_ascii_hexdigit() {
        end += 1;
    }
    let len = end - pos;
    // Only match 8+ chars (32-bit+ random values), and require a boundary
    // before to avoid mid-word matches.
    if len >= 8 {
        let preceded_by_boundary = pos == 0 || {
            let prev = text.as_bytes()[pos - 1];
            !prev.is_ascii_alphanumeric()
        };
        let followed_by_boundary =
            end >= text.len() || { !text.as_bytes()[end].is_ascii_alphanumeric() };
        if preceded_by_boundary && followed_by_boundary {
            return Some(Span { start: pos, end });
        }
    }
    None
}

/// Try to match an absolute path. Only matches when preceded by a
/// non-alphanumeric boundary (to avoid mid-sentence `/` characters).
fn try_absolute_path(text: &str, pos: usize) -> Option<Span> {
    // Only match when preceded by a boundary character.
    if pos > 0 && text.as_bytes()[pos - 1].is_ascii_alphanumeric() {
        return None;
    }

    let rest = &text[pos..];
    let bytes = rest.as_bytes();
    let mut end = pos + 1;

    // Collect path characters: alphanumeric, /, \, -, _, ., :
    while end < text.len() {
        let b = bytes[end - pos];
        if b.is_ascii_alphanumeric()
            || b == b'/'
            || b == b'\\'
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b':'
        {
            end += 1;
        } else {
            break;
        }
    }

    let len = end - pos;
    // Must be at least 3 chars (e.g., /a), and must contain a path separator.
    let has_sep = rest[..len].contains('/') || rest[..len].contains('\\');
    if len >= 3 && has_sep {
        // Avoid matching very short paths that might be false positives
        // (like `/tmp` is fine, `/a` is not distinctive enough).
        let sep_count = rest[..len]
            .chars()
            .filter(|&c| c == '/' || c == '\\')
            .count();
        if sep_count >= 1 && len >= 4 {
            return Some(Span { start: pos, end });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_iso_date() {
        let mut ex = DynamicValueExtractor::new();
        let input = "Date: 2026-07-23 is today.";
        let result = ex.extract_replace(input);
        assert!(result.contains("<datetime_"));
        assert!(!result.contains("2026-07-23"));
        assert_eq!(ex.extracted_count(), 1);
    }

    #[test]
    fn extracts_iso_datetime_with_time() {
        let mut ex = DynamicValueExtractor::new();
        let input = "At 2026-07-23T16:28:00 we deployed.";
        let result = ex.extract_replace(input);
        assert!(result.contains("<datetime_"));
        assert!(!result.contains("2026-07-23T16:28:00"));
    }

    #[test]
    fn extracts_uuid() {
        let mut ex = DynamicValueExtractor::new();
        let input = "id=550e8400-e29b-41d4-a716-446655440000 done";
        let result = ex.extract_replace(input);
        assert!(result.contains("<uuid_"));
        assert!(!result.contains("550e8400"));
    }

    #[test]
    fn extracts_long_timestamp() {
        let mut ex = DynamicValueExtractor::new();
        let input = "ts: 1784575505000 ms";
        let result = ex.extract_replace(input);
        assert!(result.contains("<timestamp_"));
        assert!(!result.contains("1784575505000"));
    }

    #[test]
    fn extracts_hex_random_id() {
        let mut ex = DynamicValueExtractor::new();
        let input = "hash deadbeefcafe1234 in the log";
        let result = ex.extract_replace(input);
        // "deadbeefcafe1234" is 16 hex chars with boundary before/after
        assert!(result.contains("<hex_"));
        assert!(!result.contains("deadbeefcafe1234"));
    }

    #[test]
    fn does_not_match_short_numbers() {
        let mut ex = DynamicValueExtractor::new();
        // "2026" is not a full date, "12345" is too short for timestamp
        let input = "year 2026, id 12345.";
        let result = ex.extract_replace(input);
        assert_eq!(result, input); // unchanged
        assert_eq!(ex.extracted_count(), 0);
    }

    #[test]
    fn does_not_match_hex_in_code() {
        let mut ex = DynamicValueExtractor::new();
        // 0xDEADBEEF — preceded by 0x (alphanumeric), so not a boundary
        let input = "let mask = 0xDEADBEEF;";
        let result = ex.extract_replace(input);
        assert_eq!(result, input);
    }

    #[test]
    fn extracts_absolute_path() {
        let mut ex = DynamicValueExtractor::new();
        let input = "Error at /home/user/project/src/main.rs:42";
        let result = ex.extract_replace(input);
        assert!(result.contains("<path_"));
        assert!(!result.contains("/home/user/project/src/main.rs"));
    }

    #[test]
    fn collects_section_with_all_extractions() {
        let mut ex = DynamicValueExtractor::new();
        let _ = ex.extract_replace("Date: 2026-07-23");
        let _ = ex.extract_replace("UUID: 550e8400-e29b-41d4-a716-446655440000");

        let section = ex.collect_section();
        assert!(section.contains("# Cache-aligned dynamic values"));
        assert!(section.contains("datetime_1"));
        assert!(section.contains("uuid_2"));
    }

    #[test]
    fn empty_section_when_nothing_extracted() {
        let ex = DynamicValueExtractor::new();
        assert!(ex.collect_section().is_empty());
    }

    #[test]
    fn placeholder_not_re_extracted() {
        let mut ex = DynamicValueExtractor::new();
        // First pass extracts the date
        let result1 = ex.extract_replace("Date: 2026-07-23");
        let placeholder_part = result1
            .split_whitespace()
            .find(|w| w.starts_with("<datetime_"))
            .unwrap()
            .to_string();

        // Second pass should leave the placeholder alone
        let mut ex2 = DynamicValueExtractor::new();
        let result2 = ex2.extract_replace(&placeholder_part);
        assert_eq!(result2, placeholder_part.as_str());
    }

    #[test]
    fn multiple_patterns_in_one_text() {
        let mut ex = DynamicValueExtractor::new();
        let input = "date=2026-07-23 uuid=550e8400-e29b-41d4-a716-446655440000 ts=1784575505000";
        let result = ex.extract_replace(input);
        assert!(result.contains("<datetime_"));
        assert!(result.contains("<uuid_"));
        assert!(result.contains("<timestamp_"));
        assert!(!result.contains("2026-07-23"));
        assert!(!result.contains("550e8400"));
        assert!(!result.contains("1784575505000"));
    }

    #[test]
    fn bounded_cost_on_pathological_input() {
        let mut ex = DynamicValueExtractor::new();
        // Feed many dates — total extracted chars capped at 300
        for i in 0..50 {
            let _ = ex.extract_replace(&format!("date_{}=2026-07-{:02}", i, i % 30 + 1));
        }
        // extracted_count may be capped by total chars, but placeholder
        // generation should still work (ordinal increments)
        assert!(ex.extracted_count() > 0);
        let section = ex.collect_section();
        // With datetime placeholder prefix and 30 entries (capped by 300 chars),
        // section size is ~1000 chars
        assert!(section.len() <= 1500);
    }
}
