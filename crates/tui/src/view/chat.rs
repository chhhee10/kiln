//! Planner chat (tui.md screen 5).

use super::hints;
use crate::app::{App, Mode};
use crate::canvas::{Canvas, St, fg};
use crate::theme::*;
use crate::util::{tail, trunc, wrap};

const THREADS_W: i32 = 40;

pub fn draw(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    let ins = app.mode == Mode::Insert;
    if app.compact() {
        if app.side == 1 && !ins {
            threads(app, c, (0, 1, w, h - 3));
        } else {
            conversation(app, c, 0, w);
        }
    } else {
        threads(app, c, (0, 1, THREADS_W, h - 3));
        conversation(app, c, THREADS_W, w - THREADS_W);
    }
    let mut hs: Vec<(&str, &str)> = if ins {
        vec![("⏎", "send"), ("esc", "normal")]
    } else {
        vec![("i", "write"), ("3", "open plan"), ("1-6", "screens"), (":", "palette")]
    };
    if app.compact() && !ins {
        hs.insert(1, ("tab", "threads"));
    }
    hints(c, app, &hs);
}

fn threads(app: &App, c: &mut Canvas, (x, y, w, h): (i32, i32, i32, i32)) {
    c.boxed(x, y, w, h, "Threads", false, None);
    let cur = app.data.chat.thread.as_deref();
    let max = ((h - 3) / 3).max(0) as usize;
    for (i, t) in app.data.threads.iter().take(max).enumerate() {
        let yy = y + 2 + i as i32 * 3;
        let on = Some(t.key.as_str()) == cur;
        let b = if on { Some(SEL) } else { None };
        if on {
            c.sel_row(x + 1, yy, w - 2, 2);
        }
        c.put(x + 3, yy, &trunc(&t.title, (w - 5) as usize), fg(TEXT).on_opt(b).bold_if(on));
        c.put(x + 3, yy + 1, &t.status, fg(t.tone.color()).on_opt(b));
    }
    if app.data.threads.is_empty() {
        c.put(x + 3, y + 2, "no threads yet · press i to start", fg(DIM));
    }
}

fn conversation(app: &App, c: &mut Canvas, x: i32, w: i32) {
    let h = c.h();
    let ins = app.mode == Mode::Insert;
    let chat = &app.data.chat;
    let ch = h - 7; // conversation box height (38 at 45 rows)
    let model = if chat.model.is_empty() { String::new() } else { format!(" · {}", chat.model) };
    let title = format!("Planner · {}{}", chat.title, model);
    c.boxed(x, 1, w, ch, &trunc(&title, 110), !ins, None);
    let ww = (w - 12).max(8) as usize;
    let mut lines: Vec<Vec<(String, St)>> = Vec::new();
    for m in &chat.msgs {
        lines.push(if m.you {
            vec![("you".into(), fg(MUTED).bold())]
        } else {
            vec![("◆ planner".into(), fg(ACCENT).bold())]
        });
        for l in wrap(&m.text, ww) {
            lines.push(vec![(l, fg(if m.you { MUTED } else { TEXT }))]);
        }
        lines.push(vec![]);
    }
    if let Some(label) = &chat.pending {
        lines.push(vec![(format!("{} {}", app.spinner(), label), fg(DIM))]);
    }
    if chat.msgs.is_empty() && chat.pending.is_none() {
        lines.push(vec![("describe a feature; the planner drafts a plan you can edit on Plans (3)".into(), fg(DIM))]);
    }
    let rows = (ch - 3).max(0) as usize;
    let start = lines.len().saturating_sub(rows);
    let vis = &lines[start..];
    let bottom = ch; // last text row is ch - 1 - 1
    for (i, segs) in vis.iter().enumerate() {
        let yy = bottom - 1 - vis.len() as i32 + i as i32;
        let mut xx = x + 4;
        for (t, st) in segs {
            xx += c.put(xx, yy, t, *st);
        }
    }
    let my = ch + 1;
    c.boxed(x, my, w, 4, "Message", ins, None);
    c.put(x + 2, my + 1, "›", fg(ACCENT).bold());
    let room = (w - 12).max(4) as usize;
    if !app.chat.input.is_empty() || ins {
        let cursor = if ins { app.blink() } else { "" };
        c.put(x + 4, my + 1, &format!("{}{}", tail(&app.chat.input, room), cursor), fg(TEXT));
    } else {
        c.put(x + 4, my + 1, &trunc("press i to write · enter sends · esc returns to normal", room), fg(DIM));
    }
    c.put(
        x + 2,
        my + 2,
        &trunc(
            &format!("{} memories in context · plan auto-updates · voice input on Android", chat.memories),
            (w - 4) as usize,
        ),
        fg(DIM),
    );
}
