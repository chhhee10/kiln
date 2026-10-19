//! Pure rendering: `render(&App, &mut Frame)`. Each screen is a port of the
//! matching branch of the prototype's `build()`; at 160×45 every coordinate
//! equals the prototype's.

mod chat;
mod creds;
mod dash;
mod login;
mod logs;
mod overlays;
mod plans;
mod run;

use crate::app::{App, Mode, Overlay, Screen, TABS};
use crate::canvas::{Canvas, fg};
use crate::theme::*;
use crate::util::display_width;
use kiln_types::PRODUCT_NAME;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Render the app into a frame.
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();
    draw(app, frame.buffer_mut(), area);
}

/// Render the app into a buffer (also used by tests).
pub fn draw(app: &App, buf: &mut Buffer, area: Rect) {
    let mut c = Canvas::new(buf, area);
    c.clear();
    match app.screen {
        Screen::Login => {
            login::draw_login(app, &mut c);
            return;
        }
        Screen::Setup => {
            login::draw_setup(app, &mut c);
            return;
        }
        _ => {}
    }
    header(app, &mut c);
    match app.screen {
        Screen::Dash => dash::draw(app, &mut c),
        Screen::Run => run::draw(app, &mut c),
        Screen::Plans => plans::draw(app, &mut c),
        Screen::Chat => chat::draw(app, &mut c),
        Screen::Logs => logs::draw(app, &mut c),
        Screen::Creds => creds::draw(app, &mut c),
        Screen::Login | Screen::Setup => {}
    }
    status_line(app, &mut c);
    if let Some(ov) = &app.overlay {
        overlays::draw(app, ov, &mut c);
    }
}

/// Render to a fresh buffer of the given size.
pub fn render_buffer(app: &App, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    draw(app, &mut buf, area);
    buf
}

pub fn clock(app: &App) -> String {
    app.now.format("%H:%M:%S").to_string()
}

/// Short host name for copy ("mac-mini").
pub fn host(app: &App) -> String {
    let h = &app.data.server.host;
    if h.is_empty() { "kilnd".into() } else { h.clone() }
}

const SHORT_TABS: [&str; 6] = ["Dash", "Run", "Plans", "Planner", "Logs", "Creds"];

fn header(app: &App, c: &mut Canvas) {
    let w = c.w();
    c.fill(0, 0, w, 1, PANEL);
    c.put(1, 0, "◆", fg(ACCENT).on(PANEL).bold());
    c.put(3, 0, PRODUCT_NAME, fg(TEXT).on(PANEL).bold());
    let compact = w < 15 + 68 + 40;
    let mut tx = if compact { 3 + display_width(PRODUCT_NAME) as i32 + 2 } else { 15 };
    for (i, (s, l)) in TABS.iter().enumerate() {
        let on = app.screen == *s;
        let label = if compact { SHORT_TABS[i] } else { l };
        let st = format!(" {} {} ", i + 1, label);
        c.put(tx, 0, &st, fg(if on { ON_ACCENT } else { MUTED }).on(if on { ACCENT } else { PANEL }).bold_if(on));
        tx += display_width(&st) as i32 + 1;
    }
    let time = clock(app);
    let h = host(app);
    let full = format!("{}  {} ● {} ", app.data.user_email, h, time);
    let mid = format!("{h} ● {time} ");
    let short = format!("● {time} ");
    let rt = [full, mid, short].into_iter().find(|s| tx + (display_width(s) as i32) < w);
    if let Some(rt) = rt {
        let x = w - display_width(&rt) as i32;
        c.put(x, 0, &rt, fg(DIM).on(PANEL));
        let dot = rt.find('●').map(|b| display_width(&rt[..b]) as i32).unwrap_or(0);
        c.put(x + dot, 0, "●", fg(PASSED).on(PANEL));
    }
}

fn status_line(app: &App, c: &mut Canvas) {
    let y = c.h() - 2;
    if let Some(m) = app.flash_msg() {
        c.put(1, y, &crate::util::trunc(&m.text, (c.w() - 2).max(0) as usize), fg(m.tone.color()));
        return;
    }
    let d = &app.data;
    let ctx = match app.screen {
        Screen::Dash => {
            let planned = d.tasks.iter().filter(|t| t.column == kiln_types::BoardColumn::Planned).count();
            let sync = match d.last_sync {
                Some(t) => {
                    let s = (app.now - t).num_seconds();
                    if s < 10 { "just now".to_string() } else { format!("{} ago", crate::util::fmt_ago(s)) }
                }
                None => "just now".to_string(),
            };
            format!("{} · {} workers running · {} queued · last sync {}", host(app), d.workers().len(), planned, sync)
        }
        Screen::Run => match app.cur() {
            Some(t) => {
                let pr = t.pr.map(|p| format!(" · draft PR #{p}")).unwrap_or_default();
                format!("{} · {}{}", t.key, t.title, pr)
            }
            None => "no task selected".into(),
        },
        Screen::Plans => match &d.plan {
            Some(p) => format!("plan “{}” · draft v{} · {}", p.title, p.version, p.status),
            None => "no plans yet".into(),
        },
        Screen::Chat => format!("planner thread · {} messages", d.chat.msgs.len()),
        Screen::Logs => format!(
            "tail -f · {} · {}",
            crate::model::ROLES[app.logs.lw].as_str(),
            if app.logs.follow { "following" } else { "paused" }
        ),
        Screen::Creds => {
            let bad = d.creds.iter().filter(|c| c.status != kiln_types::Status::Passed && !c.is_new).count();
            if bad == 0 {
                format!("{} credentials · all healthy", d.creds.len())
            } else {
                format!("{} credentials · {} needs attention", d.creds.len(), bad)
            }
        }
        Screen::Login | Screen::Setup => String::new(),
    };
    c.put(1, y, &crate::util::trunc(&ctx, (c.w() - 2).max(0) as usize), fg(DIM));
}

/// Mode chip + hints for the hint bar, shared by all screens.
pub(crate) fn hints(c: &mut Canvas, app: &App, hints: &[(&str, &str)]) {
    let (chip, bg) = match app.mode {
        Mode::Normal => ("NORMAL", ACCENT),
        Mode::Insert => ("INSERT", RUNNING),
        Mode::Filter => ("FILTER", RUNNING),
    };
    if matches!(app.overlay, Some(Overlay::Palette { .. })) {
        return;
    }
    c.hint_bar(chip, bg, hints);
}

/// Pad a percentage to the prototype's `padStart(3)+'%'`.
pub(crate) fn pct(v: f32) -> String {
    format!("{:>3}%", v.round() as i64)
}

/// Spinner for running, glyph otherwise.
pub(crate) fn glyph(app: &App, s: kiln_types::Status) -> &'static str {
    if s == kiln_types::Status::Running { app.spinner() } else { s.glyph() }
}
