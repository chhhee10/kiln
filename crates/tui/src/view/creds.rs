//! Credentials (tui.md screen 7): vault list + setup checklist, edit pane.

use super::{hints, host};
use crate::app::{App, Mode};
use crate::canvas::{Canvas, fg};
use crate::theme::*;
use crate::util::{display_width, trunc};

/// Vault list width: 84 at 160 columns; the 76-wide edit pane keeps its size
/// on narrower terminals.
fn list_w(w: i32) -> i32 {
    (w - 76).clamp(44, 84)
}

pub fn draw(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    let ins = app.mode == Mode::Insert;
    if app.compact() {
        if ins {
            edit(app, c, (0, 1, w, h - 3));
        } else {
            list(app, c, (0, 1, w, h - 3));
        }
    } else {
        list(app, c, (0, 1, list_w(w), h - 3));
        edit(app, c, (list_w(w), 1, w - list_w(w), h - 3));
    }
    if ins {
        hints(c, app, &[("type", "edit"), ("tab", "next field"), ("⏎", "save"), ("esc", "cancel")]);
    } else {
        hints(
            c,
            app,
            &[("j/k", "move"), ("⏎", "edit"), ("tab", "field"), ("t", "test"), ("n", "new"), (":", "palette")],
        );
    }
}

type R = (i32, i32, i32, i32);

fn list(app: &App, c: &mut Canvas, (x, y, w, h): R) {
    let ins = app.mode == Mode::Insert;
    c.boxed(x, y, w, h, &format!("Credentials · per-user vault on {}", host(app)), !ins, None);
    let creds = &app.data.creds;
    // Entries that fit together with the setup checklist; without it when
    // the terminal is too short for both.
    let with_checklist = ((h - 13) / 4).max(0) as usize;
    let show_checklist = !app.data.checklist.is_empty() && creds.len() <= with_checklist.min(7);
    let max_entries = if show_checklist { with_checklist.min(7) } else { (((h - 4) / 4).max(1) as usize).min(7) };
    let off = app.creds.csel.saturating_sub(max_entries - 1);
    for (i, cr) in creds.iter().enumerate().skip(off).take(max_entries) {
        let yy = y + 2 + (i - off) as i32 * 4;
        let on = i == app.creds.csel;
        let b = if on { Some(SEL) } else { None };
        if on {
            c.sel_row(x + 1, yy, w - 2, 3);
        }
        let type_w = display_width(&cr.kind_label) as i32;
        c.put(x + 3, yy, &trunc(&cr.name, (w - 8 - type_w) as usize), fg(TEXT).on_opt(b).bold());
        c.put(x + w - 3 - type_w, yy, &cr.kind_label, fg(DIM).on_opt(b));
        let vals: Vec<String> = cr.fields.iter().map(|f| f.display(false)).filter(|v| !v.is_empty()).collect();
        let vals = if vals.is_empty() { "—".to_string() } else { vals.join("  ·  ") };
        c.put(x + 3, yy + 1, &trunc(&vals, (w - 6) as usize), fg(MUTED).on_opt(b));
        c.put(x + 3, yy + 2, cr.status.glyph(), fg(status_color(cr.status)).on_opt(b));
        c.put(x + 5, yy + 2, &trunc(&cr.status_text, (w - 8) as usize), fg(status_color(cr.status)).on_opt(b));
    }
    if creds.is_empty() {
        c.put(x + 3, y + 2, "no credentials yet · press n to add one", fg(DIM));
    }
    let shown = creds.len().clamp(1, max_entries) as i32;
    let cy = y + 3 + shown * 4;
    if show_checklist && cy < y + h - 9 {
        c.put(x + 3, cy, "setup checklist", fg(DIM));
        for (i, it) in app.data.checklist.iter().take(6).enumerate() {
            let yy = cy + 1 + i as i32;
            c.put(x + 3, yy, it.status.glyph(), fg(status_color(it.status)));
            let col = if it.status == kiln_types::Status::NeedsHuman { HUMAN } else { MUTED };
            c.put(x + 5, yy, &trunc(&it.text, (w - 8) as usize), fg(col));
        }
    }
    c.put(
        x + 3,
        y + h - 2,
        &trunc("keys belong to you · workers you dispatch use your keys", (w - 6) as usize),
        fg(DIM),
    );
}

fn edit(app: &App, c: &mut Canvas, (x, y, w, h): R) {
    let ins = app.mode == Mode::Insert;
    let Some(cr) = app.data.creds.get(app.creds.csel) else {
        c.boxed(x, y, w, h, "Edit", ins, None);
        c.put(x + 2, y + 2, &trunc("press n to add a credential", (w - 4) as usize), fg(DIM));
        return;
    };
    c.boxed(x, y, w, h, &format!("Edit · {}", cr.name), ins, None);
    let x2 = x + 2;
    let fw = (w - 6).max(4);
    c.put(x2, y + 2, "provider", fg(DIM));
    c.put(x2, y + 3, &trunc(&cr.name, fw as usize), fg(TEXT).bold());
    c.put(x2, y + 5, "type", fg(DIM));
    c.put(x2, y + 6, &cr.kind_label, fg(TEXT));
    for (i, f) in cr.fields.iter().enumerate() {
        let yy = y + 8 + i as i32 * 4;
        let on = i == app.creds.cfield;
        let ed = on && ins;
        let label = if f.secret { format!("{}  · secret", f.label) } else { f.label.clone() };
        c.put(x2, yy, &label, fg(if on { ACCENT } else { MUTED }).bold_if(on));
        let bg = if on { FIELD } else { RAISED };
        c.fill(x2, yy + 1, fw, 1, bg);
        let room = (fw - 4).max(1) as usize;
        let v = f.display(ed);
        let v = if ed { crate::util::tail(&v, room) } else { trunc(&v, room) };
        let cursor = if ed { app.blink() } else { "" };
        c.put(x2 + 1, yy + 1, &format!("{v}{cursor}"), fg(TEXT).on(bg));
    }
    let ty = y + 9 + cr.fields.len() as i32 * 4;
    if app.creds.testing {
        c.put(x2, ty, &format!("{} testing connection from a worker container…", app.spinner()), fg(RUNNING));
    } else {
        c.put(x2, ty, cr.status.glyph(), fg(status_color(cr.status)));
        c.put(x2 + 2, ty, &trunc(&cr.status_text, (fw - 2) as usize), fg(status_color(cr.status)));
    }
    c.put(x2, ty + 2, " t  test connection ", fg(ON_ACCENT).on(ACCENT).bold());
    c.put(x2 + 22, ty + 2, " n  add credential ", fg(TEXT).on(FIELD));
    c.put(
        x2,
        y + h - 4,
        &trunc("stored encrypted with your login key · never sent to the planner", fw as usize),
        fg(DIM),
    );
    c.put(x2, y + h - 3, &trunc("litellm: containers reach the proxy over the tailnet", fw as usize), fg(DIM));
}
