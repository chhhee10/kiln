//! TUI palette plus the 256-color fallback.
//!
//! Rendering always uses the truecolor constants below. When the terminal
//! doesn't advertise truecolor (`COLORTERM` is not `truecolor`/`24bit`) the
//! finished frame is passed through [`downgrade`], which maps every RGB color
//! to an xterm-256 index (the palette file's explicit table first, nearest
//! cube/gray color otherwise).

use kiln_types::Status;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

pub const BG: Color = Color::Rgb(0x0f, 0x11, 0x15);
pub const PANEL: Color = Color::Rgb(0x15, 0x17, 0x1c); // header row, hint bar
pub const RAISED: Color = Color::Rgb(0x1c, 0x1f, 0x25); // dialogs, palette
pub const FIELD: Color = Color::Rgb(0x23, 0x26, 0x2d); // text inputs
pub const SEL: Color = Color::Rgb(0x1f, 0x23, 0x2a); // selected list row
pub const LINE: Color = Color::Rgb(0x2a, 0x2d, 0x34); // unfocused borders
pub const FAINT: Color = Color::Rgb(0x3a, 0x3e, 0x46); // empty bar cells ░
pub const DOTS: Color = Color::Rgb(0x1a, 0x1d, 0x22); // login backdrop dots
pub const TEXT: Color = Color::Rgb(0xec, 0xee, 0xf2);
pub const MUTED: Color = Color::Rgb(0x9a, 0x9f, 0xa8);
pub const DIM: Color = Color::Rgb(0x6b, 0x70, 0x79);
pub const ACCENT: Color = Color::Rgb(0xcd, 0xf0, 0x4a); // focused border, keys, mode chip
pub const ON_ACCENT: Color = Color::Rgb(0x14, 0x17, 0x0a);

pub const RUNNING: Color = Color::Rgb(0x5f, 0xa8, 0xf0);
pub const PASSED: Color = Color::Rgb(0x4f, 0xd1, 0x9a);
pub const FAILED: Color = Color::Rgb(0xf0, 0x60, 0x5a);
pub const HUMAN: Color = Color::Rgb(0xf2, 0xc1, 0x4e);
pub const QUEUED: Color = Color::Rgb(0x7d, 0x82, 0x8b);

/// Braille spinner, advanced every 120 ms tick.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Sub-cell progress (eighths).
pub const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
pub const SPARK: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Color of a display status.
pub fn status_color(s: Status) -> Color {
    match s {
        Status::Running => RUNNING,
        Status::Passed => PASSED,
        Status::Failed => FAILED,
        Status::NeedsHuman => HUMAN,
        Status::Queued => QUEUED,
    }
}

/// Glyph of a display status (running uses the spinner when animated).
pub fn status_glyph(s: Status) -> &'static str {
    s.glyph()
}

/// Lowercase TUI label ("needs human").
pub fn status_label(s: Status) -> &'static str {
    match s {
        Status::Running => "running",
        Status::Passed => "passed",
        Status::Failed => "failed",
        Status::NeedsHuman => "needs human",
        Status::Queued => "queued",
    }
}

/// Semantic color used by data items (inbox, activity, plan list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Status(Status),
    Accent,
    Text,
    Muted,
    Dim,
}

impl Tone {
    pub fn color(self) -> Color {
        match self {
            Tone::Status(s) => status_color(s),
            Tone::Accent => ACCENT,
            Tone::Text => TEXT,
            Tone::Muted => MUTED,
            Tone::Dim => DIM,
        }
    }
    pub const RUNNING: Tone = Tone::Status(Status::Running);
    pub const PASSED: Tone = Tone::Status(Status::Passed);
    pub const FAILED: Tone = Tone::Status(Status::Failed);
    pub const HUMAN: Tone = Tone::Status(Status::NeedsHuman);
    pub const QUEUED: Tone = Tone::Status(Status::Queued);
}

/// Log level colors (tokens.md "Log levels").
pub fn level_color(l: kiln_types::event::LogLevel) -> Color {
    use kiln_types::event::LogLevel::*;
    match l {
        Info => DIM,
        Step => TEXT,
        Pass => PASSED,
        Warn => HUMAN,
        Fail => FAILED,
        Capt => RUNNING,
    }
}

/// Which color depth to emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
}

impl ColorMode {
    /// `COLORTERM=truecolor|24bit` → truecolor, anything else → 256 colors.
    pub fn detect() -> Self {
        Self::from_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }
    pub fn from_colorterm(v: Option<&str>) -> Self {
        match v.map(|s| s.to_ascii_lowercase()) {
            Some(s) if s == "truecolor" || s == "24bit" => ColorMode::TrueColor,
            _ => ColorMode::Ansi256,
        }
    }
}

/// Explicit fallback table from the palette file.
const EXPLICIT: [((u8, u8, u8), u8); 9] = [
    ((0xcd, 0xf0, 0x4a), 191), // accent
    ((0x5f, 0xa8, 0xf0), 75),  // running
    ((0x4f, 0xd1, 0x9a), 79),  // passed
    ((0xf0, 0x60, 0x5a), 203), // failed
    ((0xf2, 0xc1, 0x4e), 221), // human
    ((0x7d, 0x82, 0x8b), 244), // queued
    ((0x0f, 0x11, 0x15), 233), // bg
    ((0x15, 0x17, 0x1c), 234), // panel
    ((0x2a, 0x2d, 0x34), 236), // line
];

/// Map an RGB color to the closest xterm-256 index.
pub fn to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if let Some((_, i)) = EXPLICIT.iter().find(|(c, _)| *c == (r, g, b)) {
        return *i;
    }
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_level = |v: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (i32::from(**l) - i32::from(v)).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest_level(r), nearest_level(g), nearest_level(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let gray_i = if avg < 8 { 0 } else { ((avg - 8) / 10).min(23) };
    let gray_v = (8 + 10 * gray_i) as u8;
    let dist = |c: (u8, u8, u8)| -> i32 {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    if dist((gray_v, gray_v, gray_v)) < dist(cube) { 232 + gray_i as u8 } else { cube_idx as u8 }
}

fn downgrade_color(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Indexed(to_ansi256(r, g, b)),
        other => other,
    }
}

/// Convert every RGB color in the buffer to its 256-color fallback.
pub fn downgrade(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        cell.fg = downgrade_color(cell.fg);
        cell.bg = downgrade_color(cell.bg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_fallbacks() {
        assert_eq!(to_ansi256(0xcd, 0xf0, 0x4a), 191);
        assert_eq!(to_ansi256(0x0f, 0x11, 0x15), 233);
        assert_eq!(to_ansi256(0x2a, 0x2d, 0x34), 236);
    }

    #[test]
    fn nearest_fallbacks() {
        assert_eq!(to_ansi256(0, 0, 0), 16);
        assert_eq!(to_ansi256(255, 255, 255), 231);
        // text #eceef2 is a light gray
        let t = to_ansi256(0xec, 0xee, 0xf2);
        assert!(t >= 252 || t == 231, "{t}");
    }

    #[test]
    fn colorterm_detection() {
        assert_eq!(ColorMode::from_colorterm(Some("truecolor")), ColorMode::TrueColor);
        assert_eq!(ColorMode::from_colorterm(Some("24bit")), ColorMode::TrueColor);
        assert_eq!(ColorMode::from_colorterm(Some("")), ColorMode::Ansi256);
        assert_eq!(ColorMode::from_colorterm(None), ColorMode::Ansi256);
    }
}
