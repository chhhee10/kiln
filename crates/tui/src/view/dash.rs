//! Dashboard (tui.md screen 2).

use super::{glyph, hints, host, pct};
use crate::app::App;
use crate::canvas::{Canvas, fg};
use crate::model::{Data, ROLES};
use crate::theme::*;
use crate::util::{display_width, harness_short, trunc};
use kiln_types::{BoardColumn, Status};

const RIGHT_W: i32 = 56;

pub fn draw(app: &App, c: &mut Canvas) {
    if app.dash_compact() {
        draw_compact(app, c);
    } else {
        draw_full(app, c);
    }
    let mut h: Vec<(&str, &str)> = vec![("j/k", "move"), ("⏎", "open run")];
    if app.dash_compact() {
        h.push(("tab", "pane"));
    }
    h.extend([("1-6", "screens"), (":", "palette"), ("?", "help"), ("q", "sign out")]);
    hints(c, app, &h);
}

fn summary(app: &App, c: &mut Canvas, x: i32, y: i32, maxw: i32) {
    let d = &app.data;
    let cnt = |s: Status| d.tasks.iter().filter(|t| t.status == s && t.column != BoardColumn::Done).count();
    let items = [
        (format!("{} tasks", d.tasks.len()), DIM),
        (format!("{} running", d.workers().len()), RUNNING),
        (format!("{} failed", cnt(Status::Failed)), FAILED),
        (format!("{} needs human", cnt(Status::NeedsHuman)), HUMAN),
        (format!("{} ready to review", d.tasks.iter().filter(|t| t.column == BoardColumn::Ready).count()), PASSED),
    ];
    let mut sx = x;
    for (i, (s, col)) in items.iter().enumerate() {
        let need = display_width(s) as i32 + if i > 0 { 2 } else { 0 };
        if sx + need > x + maxw {
            break;
        }
        if i > 0 {
            c.put(sx, y, "·", fg(FAINT));
            sx += 2;
        }
        sx += c.put(sx, y, s, fg(*col)) + 1;
    }
}

fn board(app: &App, c: &mut Canvas, w: i32) {
    c.boxed(0, 1, w, 15, "Board · all projects", false, None);
    let cw = if w >= 104 { 14 } else { ((w - 4) / 7).max(11) };
    for (i, col) in BoardColumn::ALL.iter().enumerate() {
        let x = 2 + i as i32 * cw;
        let ct: Vec<_> = app.data.tasks.iter().filter(|t| t.column == *col).collect();
        let name = col.label();
        c.put(x, 3, name, fg(TEXT).bold());
        c.put(x + display_width(name) as i32 + 1, 3, &ct.len().to_string(), fg(DIM));
        c.put(x, 4, &"─".repeat((cw - 2) as usize), fg(LINE));
        for (j, t) in ct.iter().take(7).enumerate() {
            let y = 5 + j as i32;
            c.put(x, y, glyph(app, t.status), fg(status_color(t.status)));
            let idc = if t.status == Status::Passed && *col == BoardColumn::Done { DIM } else { TEXT };
            c.put(x + 2, y, &trunc(&t.key, (cw - 4) as usize), fg(idc));
        }
        if ct.len() > 7 {
            c.put(x + 2, 12, &format!("+{} more", ct.len() - 7), fg(DIM));
        }
    }
    summary(app, c, 2, 14, w - 4);
}

fn spark(d: &Data, n: usize) -> String {
    let cpu = &d.machine.cpu;
    let start = cpu.len().saturating_sub(n);
    cpu[start..].iter().map(|v| SPARK[((v / 12.5).floor().max(0.0) as usize).min(7)]).collect()
}

fn ram_pct(d: &Data) -> f32 {
    if d.machine.ram_total_gb > 0.0 { d.machine.ram_used_gb / d.machine.ram_total_gb * 100.0 } else { 0.0 }
}

fn role_counts(d: &Data) -> String {
    let ws = d.workers();
    let rc = |i: usize| ws.iter().filter(|t| t.stage() == Some(i)).count();
    format!("builder {}   qa {}   reviewer {}", rc(0), rc(1), rc(2))
}

fn ctr_slots(c: &mut Canvas, d: &Data, x: i32, y: i32) {
    let m = &d.machine;
    let shown = m.ctr_slots.min(9) as usize;
    let used = (m.ctr_used as usize).min(shown);
    c.put(x, y, &"■ ".repeat(used), fg(RUNNING));
    c.put(x + used as i32 * 2, y, &"□ ".repeat(shown - used), fg(FAINT));
    c.put(x + 18, y, &format!("{}/{} slots", m.ctr_used, m.ctr_slots), fg(TEXT));
}

fn machine(app: &App, c: &mut Canvas, mx: i32) {
    let d = &app.data;
    let m = &d.machine;
    c.boxed(mx, 1, RIGHT_W, 15, &format!("Machine · {}", host(app)), false, None);
    let cpu_now = m.cpu.last().copied().unwrap_or(0.0);
    c.put(mx + 2, 3, "cpu", fg(DIM));
    c.put(mx + 8, 3, &spark(d, 34), fg(RUNNING));
    c.put(mx + 44, 3, &pct(cpu_now), fg(TEXT));
    c.put(mx + 2, 5, "ram", fg(DIM));
    c.bar(mx + 8, 5, 34, ram_pct(d), RUNNING);
    c.put(mx + 44, 5, &format!("{:.1}G", m.ram_used_gb), fg(TEXT));
    c.put(mx + 2, 7, "ctr", fg(DIM));
    ctr_slots(c, d, mx + 8, 7);
    c.put(mx + 2, 9, "queue", fg(DIM));
    c.put(mx + 8, 9, &format!("{} waiting · est. {}", m.queue, m.est_wait), fg(TEXT));
    c.put(mx + 2, 11, "roles", fg(DIM));
    c.put(mx + 8, 11, &role_counts(d), fg(MUTED));
    c.put(mx + 2, 13, &trunc(&m.host_line, (RIGHT_W - 4) as usize), fg(DIM));
}

/// Running workers table in a box at (0, y) of size w×h.
fn workers(app: &App, c: &mut Canvas, y0: i32, w: i32, h: i32) {
    c.boxed(0, y0, w, h, "Running workers", true, None);
    let extra = (w - 104).max(0);
    let (xs, xst, xh, xb) = (12, 48 + extra, 59 + extra, 82 + extra);
    let task_w = (34 + extra).max(8);
    let hy = y0 + 1;
    c.put(4, hy, "ID", fg(DIM));
    c.put(xs, hy, "TASK", fg(DIM));
    if w >= 100 {
        c.put(xst, hy, "STAGE", fg(DIM));
        c.put(xh, hy, "HARNESS · MODEL", fg(DIM));
        c.put(xb, hy, "PROGRESS", fg(DIM));
    }
    let ws = app.data.workers();
    let rows = (h - 4).max(0) as usize;
    let off = app.dsel.saturating_sub(rows.saturating_sub(1));
    for (i, t) in ws.iter().enumerate().skip(off).take(rows) {
        let y = y0 + 2 + (i - off) as i32;
        let on = i == app.dsel;
        let b = if on { Some(SEL) } else { None };
        if on {
            c.fill(1, y, w - 2, 1, SEL);
            c.put(1, y, "▌", fg(ACCENT).on(SEL));
        }
        let si = t.stage().unwrap_or(0);
        let wk = &t.workers[si];
        c.put(2, y, app.spinner(), fg(RUNNING).on_opt(b));
        c.put(4, y, &t.key, fg(TEXT).on_opt(b).bold_if(on));
        if w >= 100 {
            c.put(xs, y, &trunc(&t.title, task_w as usize), fg(if on { TEXT } else { MUTED }).on_opt(b));
            c.put(xst, y, ROLES[si].as_str(), fg(TEXT).on_opt(b));
            c.put(xh, y, &trunc(&format!("{} · {}", harness_short(&wk.harness), wk.model), 22), fg(DIM).on_opt(b));
            c.bar(xb, y, 14, t.progress, RUNNING);
            c.put(xb + 15, y, &pct(t.progress), fg(TEXT).on_opt(b));
        } else {
            // Narrow: id · task · role · bar · %.
            let tw = (w - 12 - 12 - 8 - 6).max(6);
            c.put(xs, y, &trunc(&t.title, tw as usize), fg(if on { TEXT } else { MUTED }).on_opt(b));
            let rx = xs + tw + 1;
            c.put(rx, y, ROLES[si].as_str(), fg(TEXT).on_opt(b));
            c.bar(w - 15, y, 8, t.progress, RUNNING);
            c.put(w - 6, y, &pct(t.progress), fg(TEXT).on_opt(b));
        }
    }
    if ws.is_empty() {
        c.put(4, y0 + 3, "no workers running · dispatch a plan with 3", fg(DIM));
    }
    c.put(
        2,
        y0 + h - 2,
        &format!("⏎ open run · j/k move · {} of {} slots in use", ws.len(), app.data.machine.ctr_slots),
        fg(DIM),
    );
}

fn inbox_rows(app: &App, c: &mut Canvas, x: i32, y: i32, w: i32, max: usize, two_line: bool) {
    for (i, it) in app.data.inbox.iter().take(max).enumerate() {
        let tw = (w - 6).max(4) as usize;
        if two_line {
            let yy = y + i as i32 * 2;
            c.put(x, yy, &it.glyph, fg(it.tone.color()).bold());
            c.put(x + 2, yy, &trunc(&it.title, tw.min(50)), fg(TEXT));
            c.put(x + 2, yy + 1, &trunc(&it.sub, tw.min(50)), fg(DIM));
        } else {
            let yy = y + i as i32;
            c.put(x, yy, &it.glyph, fg(it.tone.color()).bold());
            let n = c.put(x + 2, yy, &trunc(&it.title, tw), fg(TEXT));
            let rest = tw as i32 - n - 3;
            if rest > 6 {
                c.put(x + 2 + n + 1, yy, &trunc(&format!("· {}", it.sub), rest as usize), fg(DIM));
            }
        }
    }
    if app.data.inbox.is_empty() {
        c.put(x, y, "nothing needs you right now", fg(DIM));
    }
}

fn activity_rows(app: &App, c: &mut Canvas, y: i32, w: i32, n: usize) {
    for (i, a) in app.data.activity.iter().take(n).enumerate() {
        let yy = y + i as i32;
        c.put(2, yy, &a.at.format("%H:%M:%S").to_string(), fg(if i == 0 { MUTED } else { DIM }));
        c.put(12, yy, &trunc(&a.actor, 5), fg(MUTED));
        c.put(18, yy, &a.glyph, fg(a.tone.color()));
        c.put(20, yy, &trunc(&a.text, (w - 22).max(0) as usize), fg(if i == 0 { TEXT } else { MUTED }));
    }
}

fn draw_full(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    let lw = w - RIGHT_W;
    board(app, c, lw);
    machine(app, c, lw);
    let act_h = 11;
    let wh = h - 16 - act_h - 2;
    workers(app, c, 16, lw, wh);
    c.boxed(lw, 16, RIGHT_W, wh, "Inbox · needs you", false, None);
    inbox_rows(app, c, lw + 2, 18, RIGHT_W, ((wh - 3) / 2).max(0) as usize, true);
    let ay = 16 + wh;
    c.boxed(0, ay, w, act_h, "Activity", false, None);
    activity_rows(app, c, ay + 1, w, 9);
}

type RowFn<'a> = Box<dyn Fn(&mut Canvas, i32) + 'a>;

const SIDE: [&str; 3] = ["Machine", "Inbox", "Activity"];

fn draw_compact(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    // Board: counts only.
    c.boxed(0, 1, w, 4, "Board · all projects", false, None);
    let mut x = 2;
    for col in BoardColumn::ALL {
        let n = app.data.tasks.iter().filter(|t| t.column == col).count();
        x += c.put(x, 2, col.label(), fg(TEXT).bold()) + 1;
        x += c.put(x, 2, &n.to_string(), fg(DIM)) + 2;
    }
    summary(app, c, 2, 3, w - 4);

    let side_h = 8.min((h - 3 - 4) / 2).max(4);
    let sy = h - 2 - side_h;
    workers(app, c, 5, w, sy - 5);

    // Collapsed right-hand panes, cycled with tab.
    c.boxed(0, sy, w, side_h, "", false, None);
    let mut tx = 2;
    tx += c.put(tx, sy, " ", fg(LINE));
    for (i, name) in SIDE.iter().enumerate() {
        if i > 0 {
            tx += c.put(tx, sy, " · ", fg(FAINT));
        }
        let on = i == app.side;
        tx += c.put(tx, sy, name, fg(if on { ACCENT } else { DIM }).bold_if(on));
    }
    c.put(tx, sy, " ", fg(LINE));
    c.put(w - 8, sy, " tab ", fg(DIM));
    let inner = (side_h - 2).max(0) as usize;
    let d = &app.data;
    match app.side {
        0 => {
            let m = &d.machine;
            let rows: Vec<(&str, RowFn)> = vec![
                (
                    "cpu",
                    Box::new(|c: &mut Canvas, y| {
                        let n = (w - 18).max(4) as usize;
                        c.put(8, y, &spark(d, n), fg(RUNNING));
                        c.put(w - 7, y, &pct(m.cpu.last().copied().unwrap_or(0.0)), fg(TEXT));
                    }),
                ),
                (
                    "ram",
                    Box::new(|c: &mut Canvas, y| {
                        c.bar(8, y, (w - 18).max(4), ram_pct(d), RUNNING);
                        c.put(w - 7, y, &format!("{:.1}G", m.ram_used_gb), fg(TEXT));
                    }),
                ),
                ("ctr", Box::new(|c: &mut Canvas, y| ctr_slots(c, d, 8, y))),
                (
                    "queue",
                    Box::new(|c: &mut Canvas, y| {
                        c.put(8, y, &format!("{} waiting · est. {}", m.queue, m.est_wait), fg(TEXT));
                    }),
                ),
                (
                    "roles",
                    Box::new(|c: &mut Canvas, y| {
                        c.put(8, y, &role_counts(d), fg(MUTED));
                    }),
                ),
                (
                    "",
                    Box::new(|c: &mut Canvas, y| {
                        c.put(2, y, &trunc(&m.host_line, (w - 4) as usize), fg(DIM));
                    }),
                ),
            ];
            for (i, (label, f)) in rows.iter().take(inner).enumerate() {
                let y = sy + 1 + i as i32;
                if !label.is_empty() {
                    c.put(2, y, label, fg(DIM));
                }
                f(c, y);
            }
        }
        1 => inbox_rows(app, c, 2, sy + 1, w, inner, false),
        _ => activity_rows(app, c, sy + 1, w, inner),
    }
}
