//! Login (tui.md screen 1) and the first-run wizard (PRD §4.1), which uses the
//! same visual language: dotted backdrop, centered accent box, FIELD inputs.

use super::clock;
use crate::app::{App, SETUP_LABELS, SetupUi};
use crate::canvas::{Canvas, fg};
use crate::theme::*;
use crate::util::{display_width, tail, trunc};
use kiln_types::PRODUCT_NAME;

fn backdrop(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    for y in (1..h - 1).step_by(2) {
        for x in (2..w).step_by(4) {
            c.put(x, y, "·", fg(DOTS));
        }
    }
    c.fill(0, 0, w, 1, PANEL);
    c.put(1, 0, "◆", fg(ACCENT).on(PANEL).bold());
    c.put(3, 0, PRODUCT_NAME, fg(TEXT).on(PANEL).bold());
    let tn = if app.data.server.tailnet.is_empty() { "kilnd".to_string() } else { app.data.server.tailnet.clone() };
    let rt = format!("{tn} · {} ", clock(app));
    let x = (w - display_width(&rt) as i32 - 2).max(display_width(PRODUCT_NAME) as i32 + 5);
    c.put(x, 0, &rt, fg(DIM).on(PANEL));
}

fn reach_line(app: &App, c: &mut Canvas, x: i32, y: i32, maxw: usize) {
    let s = &app.data.server;
    let tn = if s.tailnet.is_empty() { "kilnd".to_string() } else { s.tailnet.clone() };
    match (&s.error, s.latency_ms) {
        (Some(e), _) => {
            c.put(x, y, "✕", fg(FAILED).on(BG));
            c.put(x + 2, y, &trunc(&format!("{tn} · unreachable · {e}"), maxw), fg(DIM).on(BG));
        }
        (None, Some(ms)) => {
            c.put(x, y, "●", fg(PASSED).on(BG));
            c.put(x + 2, y, &trunc(&format!("{tn} · reachable · {ms}ms"), maxw), fg(DIM).on(BG));
        }
        (None, None) => {
            c.put(x, y, app.spinner(), fg(RUNNING).on(BG));
            c.put(x + 2, y, &trunc(&format!("{tn} · checking…"), maxw), fg(DIM).on(BG));
        }
    }
}

fn input(c: &mut Canvas, app: &App, x: i32, y: i32, w: i32, value: &str, focus: bool) {
    c.fill(x, y, w, 1, FIELD);
    let room = (w - 4).max(1) as usize;
    let shown = if focus { tail(value, room) } else { trunc(value, room) };
    let cursor = if focus { app.blink() } else { "" };
    c.put(x + 1, y, &format!("{shown}{cursor}"), fg(TEXT).on(FIELD));
}

pub fn draw_login(app: &App, c: &mut Canvas) {
    backdrop(app, c);
    let (w, h) = (c.w(), c.h());
    let bx = ((w - 64) / 2).max(0);
    let by = ((h - 22) / 2 - 1).max(1);
    let bw = 64.min(w);
    c.boxed(bx, by, bw, 22, "Sign in", true, Some(BG));
    let x = bx + 4;
    let fw = (bw - 8).max(4);
    let l = &app.login;
    c.put(x, by + 2, &format!("◆ {PRODUCT_NAME}"), fg(ACCENT).on(BG).bold());
    c.put(x, by + 3, "agentic development platform", fg(DIM).on(BG));
    let lab = |f: usize| fg(if l.field == f { ACCENT } else { MUTED }).on(BG).bold_if(l.field == f);
    c.put(x, by + 5, "email", lab(0));
    input(c, app, x, by + 6, fw, &l.email, l.field == 0);
    c.put(x, by + 8, "password", lab(1));
    input(c, app, x, by + 9, fw, &"•".repeat(l.pw.chars().count()), l.field == 1);
    if l.pw.is_empty() && l.field == 1 {
        c.put(x + 3, by + 9, if app.demo { "type anything" } else { "your password" }, fg(DIM).on(FIELD));
    }
    if !l.err.is_empty() {
        c.put(x, by + 11, &trunc(&l.err, fw as usize), fg(FAILED).on(BG));
    }
    c.put(x, by + 13, " ⏎  sign in ", fg(ON_ACCENT).on(ACCENT).bold());
    if l.busy {
        c.put(x + 16, by + 13, &format!("{} signing in…", app.spinner()), fg(RUNNING).on(BG));
    } else {
        c.put(x + 16, by + 13, "tab switch field", fg(DIM).on(BG));
    }
    reach_line(app, c, x, by + 16, (fw - 2) as usize);
    let accounts = match &app.data.server.allowed_domain {
        Some(d) => format!("accounts: @{d} allowlist, or created by an admin"),
        None => "accounts: created by an admin".to_string(),
    };
    c.put(x, by + 17, &trunc(&accounts, fw as usize), fg(DIM).on(BG));
    c.put(x, by + 18, &trunc(&format!("session → {}", app.session_path), fw as usize), fg(DIM).on(BG));
    c.hint_bar("INSERT", RUNNING, &[("tab", "next field"), ("⏎", "sign in"), ("ctrl-c", "quit")]);
}

const STEPS: [&str; 3] = ["admin account", "allowed domain", "first project"];

pub fn draw_setup(app: &App, c: &mut Canvas) {
    backdrop(app, c);
    let (w, h) = (c.w(), c.h());
    let s: &SetupUi = &app.setup;
    // Field rows: 3 per field normally, 2 when the terminal is short.
    let tight = h < 34;
    let per = if tight { 2 } else { 3 };
    let max_fields = 5;
    let bh = if tight { 7 + max_fields * per + 5 } else { 7 + max_fields * per + 9 };
    let bw = 64.min(w);
    let bx = ((w - 64) / 2).max(0);
    let by = ((h - bh) / 2 - 1).max(1);
    c.boxed(bx, by, bw, bh, "First-run setup", true, Some(BG));
    let x = bx + 4;
    let fw = (bw - 8).max(4);
    c.put(x, by + 2, &format!("◆ {PRODUCT_NAME}"), fg(ACCENT).on(BG).bold());
    c.put(x, by + 3, &trunc("first-run setup · this server has no accounts yet", fw as usize), fg(DIM).on(BG));

    // Step indicator.
    let mut sx = x;
    for (i, name) in STEPS.iter().enumerate() {
        let (g, col, bold) = if i < s.step {
            ("✓", PASSED, false)
        } else if i == s.step {
            ("●", ACCENT, true)
        } else {
            ("○", DIM, false)
        };
        c.put(sx, by + 5, g, fg(col).on(BG));
        let tc = if i == s.step { TEXT } else { DIM };
        sx += 2 + c.put(sx + 2, by + 5, name, fg(tc).on(BG).bold_if(bold)) + 3;
    }

    let labels = SETUP_LABELS[s.step];
    let mut y = by + 7;
    for (i, label) in labels.iter().enumerate() {
        let on = i == s.field;
        c.put(x, y, label, fg(if on { ACCENT } else { MUTED }).on(BG).bold_if(on));
        let v = &s.fields[s.step][i];
        let shown = if SetupUi::is_secret(s.step, i) { "•".repeat(v.chars().count()) } else { v.clone() };
        input(c, app, x, y + 1, fw, &shown, on);
        y += per;
    }
    if s.step == 0 && !tight {
        c.put(x, y, "password: at least 10 characters · you become the first admin", fg(DIM).on(BG));
    }
    if s.step == 1 && !tight {
        let d = s.fields[1][0].trim().trim_start_matches('@');
        let d = if d.is_empty() { "company.dev" } else { d };
        c.put(x, y, &trunc(&format!("teammates with an @{d} email can self-register"), fw as usize), fg(DIM).on(BG));
        c.put(x, y + 1, "admins can still create accounts outside the domain", fg(DIM).on(BG));
    }
    if s.step == 2 && !tight {
        c.put(
            x,
            y,
            &trunc("the repo is mirrored on this machine; qa.yaml is read from it", fw as usize),
            fg(DIM).on(BG),
        );
    }

    let ey = by + 7 + max_fields * per;
    if !s.err.is_empty() {
        c.put(x, ey, &trunc(&s.err, fw as usize), fg(FAILED).on(BG));
    }
    let btn = match s.step {
        0 => " ⏎  continue ",
        1 => " ⏎  create admin ",
        _ => " ⏎  register project ",
    };
    let bty = ey + if tight { 1 } else { 2 };
    let bw2 = c.put(x, bty, btn, fg(ON_ACCENT).on(ACCENT).bold());
    let side = if s.busy {
        format!("{} working…", app.spinner())
    } else {
        match s.step {
            0 => "tab switch field".to_string(),
            1 => "esc back".to_string(),
            _ => "esc skip".to_string(),
        }
    };
    c.put(x + bw2 + 4, bty, &side, fg(if s.busy { RUNNING } else { DIM }).on(BG));
    if !tight {
        reach_line(app, c, x, bty + 3, (fw - 2) as usize);
        c.put(x, bty + 4, &trunc(&format!("session → {}", app.session_path), fw as usize), fg(DIM).on(BG));
    }
    let hints: &[(&str, &str)] = match s.step {
        0 => &[("tab", "next field"), ("⏎", "continue"), ("ctrl-c", "quit")],
        1 => &[("⏎", "create admin"), ("esc", "back"), ("ctrl-c", "quit")],
        _ => &[("tab", "next field"), ("⏎", "register"), ("esc", "skip"), ("ctrl-c", "quit")],
    };
    c.hint_bar("INSERT", RUNNING, hints);
}
