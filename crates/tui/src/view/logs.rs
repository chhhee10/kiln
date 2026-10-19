//! Logs (tui.md screen 6): full-screen tail with worker tabs, follow/pause
//! and a `/` filter.

use super::hints;
use crate::app::{App, Mode};
use crate::canvas::{Canvas, fg};
use crate::model::{LogLineView, ROLES};
use crate::theme::*;
use crate::util::{display_width, trunc};
use kiln_types::event::LogLevel;

/// Lines of the selected role visible under the current follow/pause state,
/// before filtering (prototype `all`), plus lines that arrived while paused.
pub fn visible_lines(app: &App) -> (Vec<&LogLineView>, usize) {
    let role = ROLES[app.logs.lw];
    let Some(key) = app.cur().map(|t| t.key.clone()) else {
        return (vec![], 0);
    };
    let Some(v) = app.data.logs.get(&key) else {
        return (vec![], 0);
    };
    let role_lines = v.iter().filter(|l| l.role == role);
    if app.logs.follow {
        (role_lines.collect(), 0)
    } else {
        let all: Vec<&LogLineView> = role_lines.collect();
        let fresh = all.iter().filter(|l| l.idx >= app.logs.frozen).count();
        (all.into_iter().filter(|l| l.idx < app.logs.frozen).collect(), fresh)
    }
}

pub fn matches_filter(l: &LogLineView, filter: &str) -> bool {
    filter.is_empty() || format!("{} {}", l.level.as_str(), l.msg).to_lowercase().contains(&filter.to_lowercase())
}

pub fn draw(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    let ins = app.mode == Mode::Filter;
    let key = app.cur().map(|t| t.key.clone()).unwrap_or_else(|| "—".into());
    c.boxed(0, 1, w, h - 3, &format!("Logs · {key} · tail -f"), true, None);
    let mut lx = 2;
    for (i, r) in ROLES.iter().enumerate() {
        let on = i == app.logs.lw;
        let s = if on { format!("[ {} ]", r.as_str()) } else { format!("  {}  ", r.as_str()) };
        c.put(lx, 2, &s, fg(if on { ACCENT } else { MUTED }).bold_if(on));
        lx += display_width(&s) as i32 + 1;
    }
    let (all, fresh) = visible_lines(app);
    let compact = w < 120;
    let fx = if compact { lx + 1 } else { 60 };
    let follow_text = if app.logs.follow {
        format!("{} following", app.spinner())
    } else if compact {
        format!("❚❚ paused · {fresh} new · f")
    } else {
        format!("❚❚ paused · {fresh} new lines · f to follow")
    };
    let fw = c.put(fx, 2, &follow_text, fg(if app.logs.follow { RUNNING } else { HUMAN }));
    let filter_x = if compact { (fx + fw + 2).max(w - 22) } else { w - 50 };
    c.put(filter_x, 2, "/", fg(ACCENT).bold());
    let f = &app.logs.filter;
    let shown = if f.is_empty() && !ins { "filter".to_string() } else { f.clone() };
    let cursor = if ins { app.blink() } else { "" };
    let room = (w - filter_x - 6).max(1) as usize;
    c.put(
        filter_x + 2,
        2,
        &format!("{}{}", crate::util::tail(&shown, room), cursor),
        fg(if !f.is_empty() || ins { TEXT } else { DIM }),
    );
    c.put(1, 3, &"─".repeat((w - 2).max(0) as usize), fg(LINE));

    let fl: Vec<&&LogLineView> = all.iter().filter(|l| matches_filter(l, f)).collect();
    let rows = (h - 8).max(0) as usize;
    let start = fl.len().saturating_sub(rows);
    let role = ROLES[app.logs.lw].as_str();
    let msg_x = if compact { 18 } else { 30 };
    for (i, l) in fl[start..].iter().enumerate() {
        let y = 4 + i as i32;
        c.put(2, y, &l.ts, fg(DIM));
        let fail = l.level == LogLevel::Fail;
        c.put(12, y, l.level.as_str(), fg(level_color(l.level)).bold_if(fail));
        if !compact {
            c.put(18, y, &format!("[{role}]"), fg(FAINT));
        }
        c.put(msg_x, y, &trunc(&l.msg, (w - msg_x - 4).max(0) as usize), fg(if fail { FAILED } else { TEXT }));
    }
    if fl.is_empty() {
        let msg = if f.is_empty() { "no log lines yet".to_string() } else { format!("no lines match “{f}”") };
        c.put(2, 6, &msg, fg(DIM));
    }
    let foot = format!(
        "{} of {} lines{} · ~/.local/share/kiln/runs/{}/{}.log",
        fl.len(),
        all.len(),
        if f.is_empty() { "" } else { " match" },
        key,
        role
    );
    c.put(2, h - 4, &trunc(&foot, (w - 4).max(0) as usize), fg(DIM));
    if ins {
        hints(c, app, &[("⏎", "apply"), ("esc", "done")]);
    } else {
        hints(c, app, &[("h/l", "worker"), ("f", "follow"), ("/", "filter"), ("G", "jump to end"), (":", "palette")]);
    }
}
