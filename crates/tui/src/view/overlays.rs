//! Overlays: command palette (72×22), help (96×33), feedback → Builder
//! (84×10), approve confirm (64×9), and phone pairing (QR).

use crate::app::{App, Overlay};
use crate::canvas::{Canvas, fg};
use crate::palette;
use crate::qr::QrGrid;
use crate::theme::*;
use crate::util::{display_width, tail, trunc};

pub fn draw(app: &App, ov: &Overlay, c: &mut Canvas) {
    match ov {
        Overlay::Palette { q, i } => draw_palette(app, c, q, *i),
        Overlay::Help => draw_help(c),
        Overlay::Feedback { buf } => draw_feedback(app, c, buf),
        Overlay::Confirm => draw_confirm(app, c),
        Overlay::Pair(p) => draw_pair(app, c, p.as_ref()),
    }
}

/// Centered origin for a w×h dialog, nudged up like the prototype
/// (`up` = rows above true center at 45 rows).
fn origin(c: &Canvas, w: i32, h: i32, up: i32) -> (i32, i32) {
    let x = ((c.w() - w) / 2).max(0);
    let y = ((c.h() - h) / 2 - up).max(1);
    (x, y)
}

fn draw_confirm(app: &App, c: &mut Canvas) {
    let w = 64.min(c.w());
    let (x, y) = origin(c, w, 9, 2);
    c.boxed(x, y, w, 9, "Approve & dispatch", true, Some(RAISED));
    let n = app.data.plan.as_ref().map(|p| p.tasks.len()).unwrap_or(0);
    c.put(x + 3, y + 2, &format!("Dispatch {n} tasks to workers?"), fg(TEXT).on(RAISED).bold());
    c.put(
        x + 3,
        y + 3,
        &trunc("builder → qa → reviewer per task · loops max 3, then escalate", (w - 6) as usize),
        fg(DIM).on(RAISED),
    );
    c.put(x + 3, y + 5, " y  dispatch ", fg(ON_ACCENT).on(ACCENT).bold());
    c.put(x + 18, y + 5, " n  cancel ", fg(TEXT).on(FIELD));
}

fn draw_feedback(app: &App, c: &mut Canvas, buf: &str) {
    let w = 84.min(c.w() - 2).max(30);
    let (x, y) = origin(c, w, 10, 2);
    let (key, att) = app.cur().map(|t| (t.key.clone(), t.attempt)).unwrap_or(("—".into(), 1));
    c.boxed(x, y, w, 10, &format!("Feedback → Builder · {key}"), true, Some(RAISED));
    c.put(
        x + 3,
        y + 2,
        &trunc(
            &format!("starts attempt {}/3 with your note + QA report + reviewer verdict", (att + 1).min(3)),
            (w - 6) as usize,
        ),
        fg(DIM).on(RAISED),
    );
    c.fill(x + 3, y + 4, w - 6, 1, FIELD);
    c.put(x + 4, y + 4, "›", fg(ACCENT).on(FIELD).bold());
    let room = (w - 12).max(1) as usize;
    c.put(x + 6, y + 4, &format!("{}{}", tail(buf, room), app.blink()), fg(TEXT).on(FIELD));
    if buf.is_empty() {
        c.put(
            x + 7,
            y + 4,
            &trunc("e.g. make the backoff cap a config value, seed the RNG", room - 1),
            fg(DIM).on(FIELD),
        );
    }
    c.put(x + 3, y + 7, "⏎", fg(ACCENT).on(RAISED).bold());
    c.put(x + 5, y + 7, "send", fg(MUTED).on(RAISED));
    c.put(x + 12, y + 7, "esc", fg(ACCENT).on(RAISED).bold());
    c.put(x + 16, y + 7, "cancel", fg(MUTED).on(RAISED));
}

const HELP: [(&str, &[(&str, &str)]); 6] = [
    (
        "global",
        &[
            ("1-6", "switch screen"),
            (": · ctrl-k", "command palette"),
            ("?", "this help"),
            ("q", "sign out"),
            ("esc", "back / normal mode"),
        ],
    ),
    ("lists", &[("j/k ↑↓", "move"), ("⏎ / l", "open"), ("tab", "next pane / field"), ("h", "back")]),
    (
        "run",
        &[
            ("r", "retry task"),
            ("s", "stop task"),
            ("f", "feedback to builder"),
            ("e", "escalate to human"),
            ("o", "open artifact externally"),
            ("L", "full logs"),
        ],
    ),
    (
        "plans",
        &[("space", "expand task"), ("h/l", "cycle model"), ("H/L", "cycle harness"), ("a", "approve & dispatch")],
    ),
    ("logs", &[("h/l", "switch worker"), ("f", "follow / pause"), ("/", "filter"), ("G", "jump to end")]),
    ("credentials", &[("⏎ / i", "edit field"), ("t", "test connection"), ("n", "add credential")]),
];

fn draw_help(c: &mut Canvas) {
    if c.w() >= 98 && c.h() >= 36 {
        let (x, y) = origin(c, 96, 33, 1);
        c.boxed(x, y, 96, 33, "Keybindings", true, Some(RAISED));
        for (gi, (name, keys)) in HELP.iter().enumerate() {
            let gx = x + 4 + (gi as i32 % 2) * 46;
            let gy = y + 2 + (gi as i32 / 2) * 10;
            c.put(gx, gy, name, fg(ACCENT).on(RAISED).bold());
            for (j, (k, d)) in keys.iter().enumerate() {
                c.put(gx, gy + 1 + j as i32, k, fg(TEXT).on(RAISED).bold());
                c.put(gx + 14, gy + 1 + j as i32, d, fg(MUTED).on(RAISED));
            }
        }
        c.put(x + 4, y + 31, "press ? or esc to close", fg(DIM).on(RAISED));
    } else {
        // Compact: three columns, two group rows.
        let w = (c.w() - 2).clamp(40, 96);
        let h = 18.min(c.h() - 2);
        let (x, y) = origin(c, w, h, 0);
        c.boxed(x, y, w, h, "Keybindings", true, Some(RAISED));
        let colw = (w - 4) / 3;
        let kw = 11;
        for (gi, (name, keys)) in HELP.iter().enumerate() {
            let gx = x + 2 + (gi as i32 % 3) * colw;
            let gy = y + 1 + (gi as i32 / 3) * 8;
            c.put(gx, gy, name, fg(ACCENT).on(RAISED).bold());
            for (j, (k, d)) in keys.iter().enumerate() {
                c.put(gx, gy + 1 + j as i32, &trunc(k, (kw - 1) as usize), fg(TEXT).on(RAISED).bold());
                c.put(gx + kw, gy + 1 + j as i32, &trunc(d, (colw - kw - 1).max(1) as usize), fg(MUTED).on(RAISED));
            }
        }
        c.put(x + 2, y + h - 2, "press ? or esc to close", fg(DIM).on(RAISED));
    }
}

fn draw_palette(app: &App, c: &mut Canvas, q: &str, sel: usize) {
    let cmds = palette::commands(app, q);
    let w = 72.min(c.w());
    let h = 22.min(c.h() - 1);
    let (x, y) = origin(c, w, 22.min(h), 5);
    c.boxed(x, y, w, h, "Command palette", true, Some(RAISED));
    c.put(x + 2, y + 2, "›", fg(ACCENT).on(RAISED).bold());
    c.put(x + 4, y + 2, &format!("{}{}", tail(q, (w - 20) as usize), app.blink()), fg(TEXT).on(RAISED));
    if q.is_empty() {
        c.put(x + 5, y + 2, "type a command, screen or task id", fg(DIM).on(RAISED));
    }
    let count = format!("{} cmds", cmds.len());
    c.put(x + w - 11, y + 2, &count, fg(DIM).on(RAISED));
    c.put(x + 1, y + 3, &"─".repeat((w - 2) as usize), fg(LINE).on(RAISED));
    let rows = ((h - 10).max(1) as usize).min(palette::MAX_ROWS);
    let off = sel.saturating_sub(rows - 1);
    for (i, cmd) in cmds.iter().enumerate().skip(off).take(rows) {
        let yy = y + 4 + (i - off) as i32;
        let on = i == sel;
        let b = if on { SEL } else { RAISED };
        c.fill(x + 1, yy, w - 2, 1, b);
        c.put(x + 2, yy, if on { "▸" } else { " " }, fg(ACCENT).on(b).bold());
        c.put(x + 4, yy, cmd.group, fg(DIM).on(b));
        c.put(
            x + 13,
            yy,
            &trunc(&cmd.label, (w - 26).max(4) as usize),
            fg(if on { TEXT } else { MUTED }).on(b).bold_if(on),
        );
        if !cmd.key.is_empty() {
            c.put(x + w - 3 - display_width(cmd.key) as i32, yy, cmd.key, fg(ACCENT).on(b).bold());
        }
    }
    if cmds.is_empty() {
        c.put(x + 4, y + 5, "no matches", fg(DIM).on(RAISED));
    }
    c.put(x + 2, y + h - 2, "↑↓ ctrl-n/p select · ⏎ run · esc close", fg(DIM).on(RAISED));
    c.hint_bar("PALETTE", ACCENT, &[("type", "filter"), ("↑↓", "select"), ("⏎", "run"), ("esc", "close")]);
}

fn draw_pair(app: &App, c: &mut Canvas, p: Option<&crate::model::PairView>) {
    let Some(p) = p else {
        let (x, y) = origin(c, 56, 7, 2);
        c.boxed(x, y, 56, 7, "Pair a phone", true, Some(RAISED));
        c.put(x + 3, y + 3, &format!("{} requesting a pairing code…", app.spinner()), fg(RUNNING).on(RAISED));
        return;
    };
    let grid = QrGrid::encode(&p.uri);
    let qr_rows = grid.as_ref().map(|g| g.rows() as i32).unwrap_or(0);
    let qr_w = grid.as_ref().map(|g| g.size as i32).unwrap_or(0);
    let fits = grid.is_some() && qr_rows + 9 < c.h() && qr_w + 6 <= c.w();
    let w = if fits { (qr_w + 8).max(60) } else { 60.min(c.w()) };
    let h = if fits { qr_rows + 9 } else { 9 };
    let (x, y) = origin(c, w, h, 1);
    c.boxed(x, y, w, h, "Pair a phone", true, Some(RAISED));
    let mut ty = y + 2;
    if let (true, Some(g)) = (fits, grid.as_ref()) {
        let qx = x + (w - qr_w) / 2;
        for r in 0..g.rows() {
            for (i, (top, bottom)) in g.row(r).into_iter().enumerate() {
                let col = |d: bool| if d { BG } else { TEXT };
                c.put(qx + i as i32, ty + r as i32, "▀", fg(col(top)).on(col(bottom)));
            }
        }
        ty += qr_rows + 1;
    } else {
        c.put(x + 3, ty, "terminal too small for the QR · enter the code instead", fg(HUMAN).on(RAISED));
        ty += 2;
    }
    c.put(x + 3, ty, "code", fg(DIM).on(RAISED));
    c.put(x + 9, ty, &p.code, fg(ACCENT).on(RAISED).bold());
    c.put(
        x + 3,
        ty + 1,
        &trunc(
            &format!("scan with the {} app · one-time · expires {}", kiln_types::PRODUCT_NAME, p.expires),
            (w - 6) as usize,
        ),
        fg(DIM).on(RAISED),
    );
    c.put(x + 3, ty + 2, &trunc(&p.uri, (w - 6) as usize), fg(DIM).on(RAISED));
    c.put(x + 3, ty + 4, "esc", fg(ACCENT).on(RAISED).bold());
    c.put(x + 7, ty + 4, "close", fg(MUTED).on(RAISED));
}
