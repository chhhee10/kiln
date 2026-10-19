//! Helpers for snapshot-style tests and the `snapshot` example: build an app
//! populated from the demo world and read rendered buffers back as text.

use crate::app::{App, Msg, Screen};
use crate::demo::DemoWorld;
use crate::model::DataEvent;
use chrono::TimeZone;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

/// A signed-in demo app on `screen`, sized `w`×`h`, with a fixed clock.
pub fn demo_app(screen: Screen, w: u16, h: u16) -> App {
    let mut app = App::new(true);
    app.login.email = "alex@company.dev".into();
    app.run_key = Some("FP-142".into());
    app.followed = Some("FP-142".into());
    // Initial selections of the prototype's state.
    app.dsel = 2;
    app.creds.csel = 3;
    let world = DemoWorld::new(false);
    for e in world.initial() {
        app.update(Msg::Data(e));
    }
    app.update(Msg::Data(DataEvent::SignedIn { email: "alex@company.dev".into(), flash: None }));
    app.update(Msg::Resize(w, h));
    app.now = chrono::Local.with_ymd_and_hms(2026, 10, 19, 12, 4, 31).single().unwrap_or_else(chrono::Local::now);
    app.screen = screen;
    app.mode = match screen {
        Screen::Login | Screen::Setup => crate::app::Mode::Insert,
        _ => crate::app::Mode::Normal,
    };
    app
}

/// Row `y` as a string (one char per cell; wide-glyph spill cells skipped).
pub fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()
}

/// All rows.
pub fn lines(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height).map(|y| row(buf, y)).collect()
}

/// `n` cells starting at (x, y).
pub fn text_at(buf: &Buffer, x: u16, y: u16, n: u16) -> String {
    (x..(x + n).min(buf.area.width)).map(|i| buf[(i, y)].symbol().to_string()).collect()
}

pub fn fg_at(buf: &Buffer, x: u16, y: u16) -> Color {
    buf[(x, y)].fg
}

pub fn bg_at(buf: &Buffer, x: u16, y: u16) -> Color {
    buf[(x, y)].bg
}

pub fn bold_at(buf: &Buffer, x: u16, y: u16) -> bool {
    buf[(x, y)].modifier.contains(Modifier::BOLD)
}

/// Find the first (x, y) where `needle` starts.
pub fn find(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    for y in 0..buf.area.height {
        let cells: Vec<String> = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        let joined: String = cells.concat();
        if let Some(byte) = joined.find(needle) {
            // Map byte offset back to a cell index.
            let mut acc = 0;
            for (i, c) in cells.iter().enumerate() {
                if acc == byte {
                    return Some((i as u16, y));
                }
                acc += c.len();
            }
        }
    }
    None
}
