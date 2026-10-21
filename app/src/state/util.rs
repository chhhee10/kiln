//! Pure formatting helpers shared by every surface.

use chrono::{DateTime, Utc};
use kiln_types::Project;

/// "4m12s", "0m48s", "1h03m".
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// "0:48" style for video lengths.
pub fn fmt_clock(secs: i64) -> String {
    format!("{:02}:{:02}", secs.max(0) / 60, secs.max(0) % 60)
}

/// "just now", "12m ago", "2h ago", "yesterday", "2d ago", "1w ago".
pub fn fmt_ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = (now - at).num_seconds().max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        86_400..=172_799 => "yesterday".into(),
        172_800..=604_799 => format!("{}d ago", s / 86_400),
        _ => format!("{}w ago", s / 604_800),
    }
}

/// Short wait estimate: "~4m", "~45s".
pub fn fmt_wait(secs: u32) -> String {
    if secs >= 60 {
        format!("~{}m", secs.div_ceil(60))
    } else {
        format!("~{secs}s")
    }
}

/// Uptime like "18d", "5h", "12m".
pub fn fmt_uptime(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// Local wall-clock "12:01".
pub fn fmt_hhmm(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// Local "12:01:03" for log lines.
pub fn fmt_hhmmss(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}

/// Short project label used on cards and filters: "Acme CLI/SDK" →
/// "CLI/SDK", "Marketing website" → "Website". One-word names stay as is.
pub fn project_short(p: &Project) -> String {
    short_name(&p.name)
}

pub fn short_name(name: &str) -> String {
    let last = name.split_whitespace().last().unwrap_or(name);
    let mut c = last.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => name.to_string(),
    }
}

/// "Alex Rivera" → "Alex R."
pub fn short_person(name: &str) -> String {
    let mut parts = name.split_whitespace();
    match (parts.next(), parts.last()) {
        (Some(first), Some(last)) => format!("{first} {}.", last.chars().next().unwrap_or(' ')),
        (Some(first), None) => first.to_string(),
        _ => name.to_string(),
    }
}

pub fn first_name(name: &str) -> String {
    name.split_whitespace().next().unwrap_or(name).to_string()
}

/// "claude-opus-5-5" → "opus-5-5" (reviewer verdict header).
pub fn short_model(model: &str) -> String {
    model.strip_prefix("claude-").unwrap_or(model).to_string()
}

/// One decimal, no trailing ".0" dropped (design shows "14.6").
pub fn fmt_gb(v: f32) -> String {
    format!("{v:.1}")
}

pub fn pct(part: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        (part / total * 100.0).clamp(0.0, 100.0)
    }
}

/// WebSocket reconnect backoff before attempt `n` (0-based): 0.5 s
/// doubling, capped at 15 s.
pub fn backoff_ms(attempt: u32) -> u32 {
    500u32.saturating_mul(1 << attempt.min(5)).min(15_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let v: Vec<u32> = (0..8).map(backoff_ms).collect();
        assert_eq!(v, vec![500, 1000, 2000, 4000, 8000, 15_000, 15_000, 15_000]);
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(252), "4m12s");
        assert_eq!(fmt_duration(48), "0m48s");
        assert_eq!(fmt_duration(3780), "1h03m");
        assert_eq!(fmt_clock(48), "00:48");
        assert_eq!(fmt_wait(240), "~4m");
        assert_eq!(fmt_wait(30), "~30s");
    }

    #[test]
    fn names() {
        assert_eq!(short_name("Acme CLI/SDK"), "CLI/SDK");
        assert_eq!(short_name("Acme Cloud"), "Cloud");
        assert_eq!(short_name("Marketing website"), "Website");
        assert_eq!(short_name("kiln"), "Kiln");
        assert_eq!(short_person("Alex Rivera"), "Alex R.");
        assert_eq!(short_model("claude-opus-5-5"), "opus-5-5");
        assert_eq!(short_model("gpt-5"), "gpt-5");
    }

    #[test]
    fn ago() {
        let now = Utc::now();
        assert_eq!(fmt_ago(now, now), "just now");
        assert_eq!(fmt_ago(now - chrono::Duration::minutes(12), now), "12m ago");
        assert_eq!(fmt_ago(now - chrono::Duration::hours(30), now), "yesterday");
        assert_eq!(fmt_ago(now - chrono::Duration::days(2), now), "2d ago");
        assert_eq!(fmt_ago(now - chrono::Duration::days(14), now), "2w ago");
    }
}
