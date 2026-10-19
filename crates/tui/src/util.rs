//! Text helpers shared by every screen (ports of the prototype's `trunc`,
//! `wrap`, `mask`).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate to at most `n` display cells, ending in `…` when cut.
pub fn trunc(s: &str, n: usize) -> String {
    if display_width(s) <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > n - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Keep the last `n` display cells (for input fields that scroll left).
pub fn tail(s: &str, n: usize) -> String {
    if display_width(s) <= n {
        return s.to_string();
    }
    let mut out: Vec<char> = Vec::new();
    let mut w = 0;
    for ch in s.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > n {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.into_iter().rev().collect()
}

/// Greedy word wrap at `w` cells; `\n` starts a new paragraph.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for par in s.split('\n') {
        let mut line = String::new();
        for word in par.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if display_width(&candidate) > w && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
            // Hard-break words longer than the line.
            while display_width(&line) > w && w > 0 {
                let head: String = take_width(&line, w);
                let rest = line[head.len()..].to_string();
                out.push(head);
                line = rest;
            }
        }
        out.push(line);
    }
    out
}

fn take_width(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut cw = 0;
    for ch in s.chars() {
        let c = ch.width().unwrap_or(0);
        if cw + c > w {
            break;
        }
        out.push(ch);
        cw += c;
    }
    out
}

/// Secret masking. While editing only the last typed character is visible;
/// otherwise long values show a 7-char prefix and the last 4.
pub fn mask(v: &str, editing: bool) -> String {
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();
    if editing {
        let mut s = "•".repeat(n.saturating_sub(1));
        if let Some(last) = chars.last() {
            s.push(*last);
        }
        s
    } else if n > 10 {
        let head: String = chars[..7].iter().collect();
        let last: String = chars[n - 4..].iter().collect();
        format!("{head}••••••••••{last}")
    } else {
        "•".repeat(n)
    }
}

/// `3214` → `3,214`.
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `252` → `4m12s`.
pub fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// `840` → `14m`, `7200` → `2h`.
pub fn fmt_ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// `412_000` → `412 KB`.
pub fn fmt_size(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.1} GB", b / KB / KB / KB)
    }
}

/// Display label of a harness id ("claude_sdk" → "Claude Agent SDK").
pub fn harness_label(id: &str) -> String {
    match id {
        "claude_sdk" => "Claude Agent SDK".into(),
        "codex" => "Codex".into(),
        "opencode" => "OpenCode".into(),
        "mock" => "Mock".into(),
        other => other.to_string(),
    }
}

/// Short harness name for the running-workers table ("claude").
pub fn harness_short(id: &str) -> String {
    match id {
        "claude_sdk" => "claude".into(),
        other => other.to_lowercase(),
    }
}

/// Harness slug for the run stage rows ("claude-agent-sdk").
pub fn harness_slug(id: &str) -> String {
    match id {
        "claude_sdk" => "claude-agent-sdk".into(),
        other => other.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunc_adds_ellipsis() {
        assert_eq!(trunc("hello", 5), "hello");
        assert_eq!(trunc("hello world", 6), "hello…");
        assert_eq!(trunc("abc", 0), "");
    }

    #[test]
    fn wrap_words() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn masked_input_shows_only_last_typed_char() {
        assert_eq!(mask("sk-abc", true), "•••••c");
        assert_eq!(mask("x", true), "x");
        assert_eq!(mask("", true), "");
        assert_eq!(mask("sk-ant-api03-Zq8mN4f2a", false), "sk-ant-••••••••••4f2a");
        assert_eq!(mask("short", false), "•••••");
    }

    #[test]
    fn numbers() {
        assert_eq!(thousands(3214), "3,214");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(fmt_dur(252), "4m12s");
        assert_eq!(fmt_size(412_000), "412 KB");
        assert_eq!(fmt_size(6_100_000), "6.1 MB");
    }
}
