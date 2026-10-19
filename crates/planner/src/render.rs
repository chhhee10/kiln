//! Small text helpers shared by the templates.

use std::fmt::Write as _;

/// Collapse whitespace to single spaces.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `s` to at most `max` characters, appending `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `1. a\n2. b\n` — multi-line items are flattened.
pub(crate) fn numbered(items: &[String]) -> String {
    let mut s = String::new();
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(s, "{}. {}", i + 1, one_line(item));
    }
    s
}

/// `- a\n- b\n`, or `fallback` when empty.
pub(crate) fn bullets(items: &[String], fallback: &str) -> String {
    if items.is_empty() {
        return format!("{fallback}\n");
    }
    let mut s = String::new();
    for item in items {
        let _ = writeln!(s, "- {}", one_line(item));
    }
    s
}

/// A code fence long enough not to be closed by anything inside `body`.
pub(crate) fn fenced(lang: &str, body: &str) -> String {
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{lang}\n{}\n{fence}\n", body.trim_end())
}

/// Trimmed text, or `fallback` when blank.
pub(crate) fn or_none(s: &str, fallback: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(numbered(&["a\n b".into(), "c".into()]), "1. a b\n2. c\n");
        assert_eq!(bullets(&[], "(none)"), "(none)\n");
        assert_eq!(fenced("", "x ``` y"), "````\nx ``` y\n````\n");
        assert_eq!(or_none("  ", "-"), "-");
    }
}
