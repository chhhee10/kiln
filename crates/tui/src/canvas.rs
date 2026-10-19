//! Cell-grid drawing primitives that mirror the prototype's `put` / `fill` /
//! `box` / `bar` helpers (design/designs/TUI.dc.html `build()`), so layout
//! coordinates map 1:1 onto the Ratatui buffer.

use crate::theme::*;
use crate::util::{display_width, trunc};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthChar;

/// Style for one `put`: foreground, optional background (otherwise the cell's
/// background is kept, as in the prototype) and weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct St {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
}

pub fn fg(c: Color) -> St {
    St { fg: Some(c), bg: None, bold: false }
}

impl St {
    pub fn on(mut self, bg: Color) -> St {
        self.bg = Some(bg);
        self
    }
    pub fn on_opt(mut self, bg: Option<Color>) -> St {
        if bg.is_some() {
            self.bg = bg;
        }
        self
    }
    pub fn bold(mut self) -> St {
        self.bold = true;
        self
    }
    pub fn bold_if(mut self, b: bool) -> St {
        self.bold = b;
        self
    }
}

pub struct Canvas<'a> {
    pub buf: &'a mut Buffer,
    pub area: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut Buffer, area: Rect) -> Self {
        Self { buf, area }
    }

    pub fn w(&self) -> i32 {
        i32::from(self.area.width)
    }
    pub fn h(&self) -> i32 {
        i32::from(self.area.height)
    }

    fn cell(&mut self, x: i32, y: i32) -> Option<&mut ratatui::buffer::Cell> {
        if x < 0 || y < 0 || x >= self.w() || y >= self.h() {
            return None;
        }
        let px = self.area.x + x as u16;
        let py = self.area.y + y as u16;
        self.buf.cell_mut((px, py))
    }

    /// Clear the whole area to the app background.
    pub fn clear(&mut self) {
        let (w, h) = (self.w(), self.h());
        for y in 0..h {
            for x in 0..w {
                if let Some(c) = self.cell(x, y) {
                    c.reset();
                    c.set_symbol(" ");
                    c.fg = TEXT;
                    c.bg = BG;
                }
            }
        }
    }

    /// Write `s` starting at (x, y). Returns the display width written.
    pub fn put(&mut self, x: i32, y: i32, s: &str, st: St) -> i32 {
        let mut cx = x;
        for ch in s.chars() {
            let cw = ch.width().unwrap_or(0) as i32;
            if cw == 0 {
                continue;
            }
            if let Some(c) = self.cell(cx, y) {
                c.set_char(ch);
                if let Some(f) = st.fg {
                    c.fg = f;
                }
                if let Some(b) = st.bg {
                    c.bg = b;
                }
                if st.bold {
                    c.modifier = Modifier::BOLD;
                } else {
                    c.modifier = Modifier::empty();
                }
            }
            if cw == 2 {
                // Wide glyph: the following cell is covered.
                let bg = st.bg;
                if let Some(c) = self.cell(cx + 1, y) {
                    c.set_symbol("");
                    if let Some(b) = bg {
                        c.bg = b;
                    }
                }
            }
            cx += cw;
        }
        cx - x
    }

    /// Fill a rectangle with spaces on `bg`.
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Color) {
        for j in y..y + h {
            for i in x..x + w {
                if let Some(c) = self.cell(i, j) {
                    c.set_symbol(" ");
                    c.bg = bg;
                    c.modifier = Modifier::empty();
                }
            }
        }
    }

    /// Rounded box with an optional ` title ` in the top border.
    #[allow(clippy::too_many_arguments)] // mirrors the prototype's box(x,y,w,h,title,focus,bg)
    pub fn boxed(&mut self, x: i32, y: i32, w: i32, h: i32, title: &str, focus: bool, bg: Option<Color>) {
        if w < 2 || h < 2 {
            return;
        }
        let c = if focus { ACCENT } else { LINE };
        if let Some(b) = bg {
            self.fill(x, y, w, h, b);
        }
        let st = fg(c).on_opt(bg);
        let horiz = "─".repeat((w - 2) as usize);
        self.put(x, y, &format!("╭{horiz}╮"), st);
        for j in 1..h - 1 {
            self.put(x, y + j, "│", st);
            self.put(x + w - 1, y + j, "│", st);
        }
        self.put(x, y + h - 1, &format!("╰{horiz}╯"), st);
        if !title.is_empty() && w > 6 {
            let t = trunc(title, (w - 6) as usize);
            self.put(x + 2, y, &format!(" {t} "), fg(if focus { ACCENT } else { MUTED }).on_opt(bg).bold_if(focus));
        }
    }

    /// Progress bar with partial blocks for sub-cell precision.
    pub fn bar(&mut self, x: i32, y: i32, w: i32, pct: f32, c: Color) {
        let (full, part, empty) = bar_cells(w, pct);
        let s = format!("{}{}", "█".repeat(full), part);
        self.put(x, y, &s, fg(c));
        let used = full as i32 + i32::from(!part.is_empty());
        self.put(x + used, y, &"░".repeat(empty), fg(FAINT));
    }

    /// Selection row: SEL fill, `▌` accent in the first column.
    pub fn sel_row(&mut self, x: i32, y: i32, w: i32, rows: i32) {
        self.fill(x, y, w, rows, SEL);
        for j in 0..rows {
            self.put(x, y + j, "▌", fg(ACCENT).on(SEL));
        }
    }

    /// Bottom hint bar: mode chip then `key desc` pairs. Truncates to the
    /// pairs that fit and appends `? more` when something was cut.
    pub fn hint_bar(&mut self, chip: &str, chip_bg: Color, hints: &[(&str, &str)]) {
        let y = self.h() - 1;
        let w = self.w();
        self.fill(0, y, w, 1, PANEL);
        self.put(0, y, &format!(" {chip} "), fg(ON_ACCENT).on(chip_bg).bold());
        let start = display_width(chip) as i32 + 4;
        let total: i32 = hints.iter().map(|(k, d)| pair_width(k, d)).sum::<i32>() - 3;
        let fits_all = start + total <= w;
        let more_w = pair_width("?", "more") - 3;
        let mut hx = start;
        let mut cut = false;
        for (k, d) in hints {
            let pw = pair_width(k, d) - 3;
            if !fits_all && hx + pw > w - more_w - 3 {
                cut = true;
                break;
            }
            self.put(hx, y, k, fg(ACCENT).on(PANEL).bold());
            self.put(hx + display_width(k) as i32 + 1, y, d, fg(MUTED).on(PANEL));
            hx += pw + 3;
        }
        if cut || !fits_all {
            self.put(hx, y, "?", fg(ACCENT).on(PANEL).bold());
            self.put(hx + 2, y, "more", fg(MUTED).on(PANEL));
        }
    }
}

fn pair_width(k: &str, d: &str) -> i32 {
    (display_width(k) + display_width(d) + 4) as i32
}

/// (full cells, partial glyph, empty cells) for a `w`-cell bar at `pct`.
pub fn bar_cells(w: i32, pct: f32) -> (usize, &'static str, usize) {
    let w = w.max(0) as usize;
    let tot = pct.clamp(0.0, 100.0) / 100.0 * w as f32;
    let full = (tot.floor() as usize).min(w);
    let part = if full < w { PARTIAL[(((tot - full as f32) * 8.0).floor() as usize).min(7)] } else { "" };
    let used = full + usize::from(!part.is_empty());
    (full, part, w.saturating_sub(used))
}

/// Render a bar to a plain string (tests, previews).
pub fn bar_string(w: i32, pct: f32) -> String {
    let (full, part, empty) = bar_cells(w, pct);
    format!("{}{}{}", "█".repeat(full), part, "░".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_blocks() {
        assert_eq!(bar_string(10, 0.0), "░░░░░░░░░░");
        assert_eq!(bar_string(10, 100.0), "██████████");
        assert_eq!(bar_string(10, 50.0), "█████░░░░░");
        // 48% of 14 cells = 6.72 → 6 full + floor(.72*8)=5 → ▋
        assert_eq!(bar_string(14, 48.0), "██████▋░░░░░░░");
        // 22% of 14 = 3.08 → 3 full, part index 0 = "" → no partial cell
        assert_eq!(bar_string(14, 22.0), "███░░░░░░░░░░░");
        // 58% of 9 = 5.22 → 5 full + ▏
        assert_eq!(bar_string(9, 58.0), "█████▏░░░");
        for pct in [0.0, 3.3, 12.5, 48.0, 99.9, 100.0, 140.0, -3.0] {
            assert_eq!(bar_string(40, pct).chars().count(), 40, "{pct}");
        }
    }

    #[test]
    fn eighths_progress() {
        for (i, part) in PARTIAL.iter().enumerate().skip(1) {
            let pct = i as f32 * 12.5 / 10.0; // i eighths of one cell in a 10-wide bar
            let s = bar_string(10, pct + 0.001);
            assert!(s.starts_with(part), "{i}: {s}");
        }
    }
}
