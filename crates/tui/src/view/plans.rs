//! Plans (tui.md screen 4): plan list + editable plan tree.

use super::hints;
use crate::app::App;
use crate::canvas::{Canvas, fg};
use crate::model::{PlanItem, ROLES};
use crate::theme::*;
use crate::util::{display_width, harness_label, trunc};

const LIST_W: i32 = 46;

pub fn draw(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    if app.compact() {
        if app.side == 1 {
            list(app, c, (0, 1, w, h - 3));
        } else {
            tree(app, c, (0, 1, w, h - 3));
        }
    } else {
        list(app, c, (0, 1, LIST_W, h - 3));
        tree(app, c, (LIST_W, 1, w - LIST_W, h - 3));
    }
    let mut hs: Vec<(&str, &str)> =
        vec![("j/k", "move"), ("space", "expand"), ("h/l", "model"), ("H/L", "harness"), ("a", "approve & dispatch")];
    if app.compact() {
        hs.push(("tab", "plans"));
    }
    hs.push((":", "palette"));
    hints(c, app, &hs);
}

type R = (i32, i32, i32, i32);

fn harness_name(app: &App, id: &str) -> String {
    app.data.catalog.iter().find(|h| h.id == id).map(|h| h.label.clone()).unwrap_or_else(|| harness_label(id))
}

fn list(app: &App, c: &mut Canvas, (x, y, w, h): R) {
    c.boxed(x, y, w, h, "Plans", false, None);
    let cur = app.data.plan.as_ref().map(|p| p.key.as_str());
    let max = ((h - 5) / 3).max(0) as usize;
    for (i, p) in app.data.plans.iter().take(max).enumerate() {
        let yy = y + 2 + i as i32 * 3;
        let on = Some(p.key.as_str()) == cur;
        let b = if on { Some(SEL) } else { None };
        if on {
            c.sel_row(x + 1, yy, w - 2, 2);
        }
        c.put(x + 3, yy, &p.glyph, fg(p.tone.color()).on_opt(b));
        c.put(x + 5, yy, &trunc(&p.title, (w - 7) as usize), fg(TEXT).on_opt(b).bold_if(on));
        let sub = format!("{} · ", p.sub);
        c.put(x + 5, yy + 1, &sub, fg(DIM).on_opt(b));
        c.put(x + 5 + display_width(&sub) as i32, yy + 1, &p.status, fg(p.tone.color()).on_opt(b));
    }
    if app.data.plans.is_empty() {
        c.put(x + 3, y + 2, "no plans yet", fg(DIM));
    }
    c.put(x + 3, y + h - 3, "n new idea → planner (4)", fg(DIM));
}

/// A rendered row of the tree: either a header/gap or a selectable item.
enum Row {
    Gap,
    Header(&'static str),
    WorkersHeader,
    Item(usize, PlanItem),
}

fn tree(app: &App, c: &mut Canvas, (x, y, w, h): R) {
    let Some(plan) = &app.data.plan else {
        c.boxed(x, y, w, h, "Plan", true, None);
        c.put(
            x + 2,
            y + 2,
            &trunc("no plan open · press n to describe an idea to the planner", (w - 4) as usize),
            fg(DIM),
        );
        return;
    };
    c.boxed(x, y, w, h, &format!("Plan · draft v{}", plan.version), true, None);
    let x2 = x + 2;
    let tw = (w - 6) as usize;
    c.put(x2, y + 2, &trunc(&plan.title, tw), fg(TEXT).bold());
    c.put(x2, y + 3, &trunc(&format!("{} · {}", plan.project, plan.byline), tw), fg(DIM));
    c.put(x2, y + 4, &trunc(&plan.description, (w - 6).min(108) as usize), fg(MUTED));

    // Build the virtual row list (prototype: gaps between tasks, section headers).
    let items = plan.items();
    let mut rows: Vec<Row> = Vec::new();
    let mut last_ti = None;
    for (idx, it) in items.iter().enumerate() {
        let ti = match it {
            PlanItem::Task(t) | PlanItem::Criterion(t, _) | PlanItem::Worker(t, _) => *t,
        };
        if last_ti.is_some() && last_ti != Some(ti) {
            rows.push(Row::Gap);
        }
        last_ti = Some(ti);
        match it {
            PlanItem::Criterion(_, 0) => rows.push(Row::Header("acceptance criteria")),
            PlanItem::Worker(_, 0) => rows.push(Row::WorkersHeader),
            _ => {}
        }
        rows.push(Row::Item(idx, *it));
    }
    let top = y + 6;
    let visible = (h - 10).max(1) as usize; // rows 7..=38 at 45 rows
    let sel_row = rows.iter().position(|r| matches!(r, Row::Item(i, _) if *i == app.pcur)).unwrap_or(0);
    let off = sel_row.saturating_sub(visible - 1);
    let hx = x + 20; // harness column (66 at 160×45)
    let mx = x + 42; // model column (88)
    let ex = x + 66; // edit hint (112)
    for (vi, row) in rows.iter().enumerate().skip(off).take(visible) {
        let yy = top + (vi - off) as i32;
        match row {
            Row::Gap => {}
            Row::Header(t) => {
                c.put(x + 6, yy, t, fg(DIM));
            }
            Row::WorkersHeader => {
                c.put(x + 6, yy, "workers", fg(DIM));
                c.put(hx, yy, "harness", fg(FAINT));
                c.put(mx, yy, "model", fg(FAINT));
            }
            Row::Item(idx, it) => {
                let on = *idx == app.pcur;
                let b = if on { Some(SEL) } else { None };
                if on {
                    c.fill(x + 1, yy, w - 2, 1, SEL);
                    c.put(x + 1, yy, "▌", fg(ACCENT).on(SEL));
                }
                match *it {
                    PlanItem::Task(ti) => {
                        let tk = &plan.tasks[ti];
                        c.put(x2, yy, if tk.open { "▾" } else { "▸" }, fg(MUTED).on_opt(b));
                        c.put(x + 4, yy, &tk.id.to_uppercase(), fg(DIM).on_opt(b));
                        let title_w = c.put(x + 8, yy, &trunc(&tk.title, (w - 10) as usize), fg(TEXT).on_opt(b).bold());
                        if !tk.open {
                            let models: Vec<&str> =
                                tk.workers.iter().map(|w| w.model.split('-').next().unwrap_or("")).collect();
                            let s = format!("{} criteria · {}", tk.ac.len(), models.join(" / "));
                            let room = (w - 12 - title_w).max(0) as usize;
                            c.put(x + 8 + title_w + 2, yy, &trunc(&s, room), fg(DIM).on_opt(b));
                        }
                    }
                    PlanItem::Criterion(ti, ai) => {
                        c.put(x + 8, yy, "○", fg(FAINT).on_opt(b));
                        c.put(x + 10, yy, &trunc(&plan.tasks[ti].ac[ai], (w - 12) as usize), fg(MUTED).on_opt(b));
                    }
                    PlanItem::Worker(ti, wi) => {
                        let wk = &plan.tasks[ti].workers[wi];
                        c.put(x + 8, yy, ROLES[wi].as_str(), fg(TEXT).on_opt(b));
                        c.put(hx, yy, &trunc(&harness_name(app, &wk.harness), 21), fg(MUTED).on_opt(b));
                        c.put(mx, yy, &trunc(&wk.model, 23), fg(TEXT).on_opt(b));
                        if on {
                            let long = "‹ h/l model · H/L harness ›";
                            if ex + (display_width(long) as i32) < x + w {
                                c.put(ex, yy, long, fg(ACCENT).on(SEL));
                            } else {
                                c.put(x + w - 15, yy, "‹ h/l · H/L ›", fg(ACCENT).on(SEL));
                            }
                        }
                    }
                }
            }
        }
    }
    let fy = y + h - 3;
    let n = plan.tasks.len();
    c.put(
        x2,
        fy,
        &trunc(&format!("{n} tasks · up to {} worker runs · loop max 3 → escalate", n * 9), (w - 30).max(0) as usize),
        fg(DIM),
    );
    let btn = " a  approve & dispatch ";
    c.put(x + w - 3 - display_width(btn) as i32, fy, btn, fg(ON_ACCENT).on(ACCENT).bold());
}
