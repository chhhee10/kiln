//! Small helpers shared by the adapters.

use kiln_types::event::{LogLevel, RunEventKind};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Max chars kept from a tool/command output in one event.
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024;
/// Max chars of any single string inside a tool-call input.
pub const MAX_INPUT_STRING_CHARS: usize = 8 * 1024;
/// Max chars of a non-JSON stdout line turned into a log event.
pub const MAX_LOG_CHARS: usize = 4 * 1024;

/// Truncate to `max` chars (char-boundary safe), marking the cut.
pub fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((i, _)) => {
            let dropped = s[i..].chars().count();
            format!("{}… [{dropped} more chars]", &s[..i])
        }
    }
}

/// Truncate every long string leaf of a JSON value (e.g. a Write tool's
/// full file content) so events stay small.
pub fn clamp_json(v: &Value, max: usize) -> Value {
    match v {
        Value::String(s) => Value::String(truncate(s, max)),
        Value::Array(a) => Value::Array(a.iter().map(|x| clamp_json(x, max)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), clamp_json(x, max)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `v[key]` as a str, if it is one.
pub fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `v[key]` as u64 (accepts floats and numeric strings).
pub fn u(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// `v[key]` as f64.
pub fn f(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()).unwrap_or(0.0),
        Some(Value::String(s)) => s
            .parse()
            .ok()
            .filter(|x: &f64| x.is_finite())
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Text of a content value: a plain string, or an array of blocks whose
/// `text` fields are joined.
pub fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| match b {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => s(b, "text").map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A non-JSON stdout line as an INFO log event (empty lines are dropped).
pub fn plain_line(line: &str) -> Vec<RunEventKind> {
    let t = line.trim();
    if t.is_empty() {
        return vec![];
    }
    vec![RunEventKind::Log {
        level: LogLevel::Info,
        message: truncate(t, MAX_LOG_CHARS),
    }]
}

/// Parse a line as a JSON object, if it is one.
pub fn json_object(line: &str) -> Option<Value> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(t)
        .ok()
        .filter(Value::is_object)
}

/// All `*.jsonl` files at `path` (the file itself, or recursively below a
/// directory), oldest first. Missing paths give an empty list.
pub fn jsonl_files(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(p: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 16 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&p, out, depth + 1),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
                _ => {}
            }
        }
    }
    if path.is_file() {
        out.push(path.to_path_buf());
    } else if path.is_dir() {
        walk(path, &mut out, 0);
    }
    out.sort_by_key(|p| {
        (
            std::fs::metadata(p).and_then(|m| m.modified()).ok(),
            p.clone(),
        )
    });
    out
}

/// Every JSON object line of every transcript file under `path`, grouped per file.
pub fn read_jsonl(path: &Path) -> Vec<Vec<Value>> {
    jsonl_files(path)
        .into_iter()
        .filter_map(|f| std::fs::read(&f).ok())
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .lines()
                .filter_map(json_object)
                .collect()
        })
        .collect()
}

/// Quote a string as a TOML basic string (for `codex -c key=value`).
pub fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// TOML array of strings.
pub fn toml_array(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| toml_quote(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Option lookup with a default.
pub fn opt<'a>(cfg: &'a kiln_types::protocol::RunConfig, key: &str) -> Option<&'a str> {
    cfg.options
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("héllo", 10), "héllo");
        let t = truncate("ééééé", 2);
        assert!(t.starts_with("éé…"), "{t}");
        assert!(t.contains("3 more"));
    }

    #[test]
    fn toml_quoting() {
        assert_eq!(toml_quote("a\"b\\c\nd\u{7f}"), "\"a\\\"b\\\\c\\nd\\u007F\"");
        assert_eq!(toml_array(&["a", "b"]), "[\"a\",\"b\"]");
    }

    #[test]
    fn numbers_are_lenient() {
        let v = serde_json::json!({"a": 3, "b": 2.7, "c": "5", "d": -1, "e": "x"});
        assert_eq!(u(&v, "a"), 3);
        assert_eq!(u(&v, "b"), 2);
        assert_eq!(u(&v, "c"), 5);
        assert_eq!(u(&v, "d"), 0);
        assert_eq!(u(&v, "e"), 0);
        assert_eq!(u(&v, "missing"), 0);
        assert_eq!(f(&v, "b"), 2.7);
    }

    #[test]
    fn content_text_variants() {
        assert_eq!(content_text(&serde_json::json!("x")), "x");
        assert_eq!(
            content_text(&serde_json::json!([{"type":"text","text":"a"},{"type":"image"},"b"])),
            "a\nb"
        );
        assert_eq!(content_text(&Value::Null), "");
    }
}
