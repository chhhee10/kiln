//! Small shared helpers: timestamps, tokenization, vector math, blobs.

use crate::error::{MemoryError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use kiln_types::Timestamp;

/// Format a timestamp the way the store does: RFC 3339, UTC, microseconds,
/// `Z` suffix — so lexical comparison in SQL equals time comparison.
pub(crate) fn ts(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// `ts(Utc::now())`.
pub(crate) fn now_ts() -> String {
    ts(Utc::now())
}

/// Parse a stored RFC 3339 timestamp.
pub(crate) fn parse_ts(s: &str) -> Result<Timestamp> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| MemoryError::Corrupt(format!("bad timestamp {s:?}: {e}")))
}

/// Parse a uuid-backed id column.
pub(crate) fn parse_id<T: std::str::FromStr>(s: &str) -> Result<T> {
    s.parse::<T>()
        .map_err(|_| MemoryError::Corrupt(format!("bad id {s:?}")))
}

/// Lowercase alphanumeric word tokens (underscores and dashes split words;
/// code identifiers like `cargo_test` become `cargo`, `test`).
pub(crate) fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// Canonical form used to compare subjects/relations/objects: lowercase,
/// trimmed, inner whitespace collapsed, wrapping quotes/backticks and
/// trailing punctuation removed.
pub(crate) fn normalize(s: &str) -> String {
    let collapsed = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    collapsed
        .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '.' | ',' | ';' | ':' | '!'))
        .trim()
        .to_string()
}

/// Rough token estimate: ≈ 4 characters per token (`chars_per_token`).
pub(crate) fn estimate_tokens(text: &str, chars_per_token: usize) -> usize {
    text.chars().count().div_ceil(chars_per_token.max(1))
}

/// Cosine similarity; 0 for mismatched or zero-length vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// In-place L2 normalization (no-op for the zero vector).
pub(crate) fn l2_normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

/// Encode a vector as the `embedding` BLOB format (f32 little-endian).
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Decode an `embedding` BLOB (f32 little-endian). Trailing bytes that do
/// not form a whole f32 are ignored.
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Truncate to at most `max` characters, appending `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// First non-empty line, trimmed.
pub(crate) fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// `"?, ?, ?"` with `n` placeholders.
pub(crate) fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.5f32, -1.25, 3.0];
        assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
    }

    #[test]
    fn normalize_strips_noise() {
        assert_eq!(normalize("  `Cargo   Test`. "), "cargo test");
        assert_eq!(normalize("Runs_Tests_With"), "runs_tests_with");
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn token_estimate() {
        assert_eq!(estimate_tokens("abcd", 4), 1);
        assert_eq!(estimate_tokens("abcde", 4), 2);
        assert_eq!(estimate_tokens("", 4), 0);
    }
}
