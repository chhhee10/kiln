//! Live run (tui.md screen 3): stages, tests, artifacts and the log pane.

use super::{glyph, hints, pct};
use crate::app::App;
use crate::canvas::{Canvas, fg};
use crate::model::{ROLES, TaskView};
use crate::theme::*;
use crate::util::{display_width, fmt_dur, harness_slug, thousands, trunc};
use kiln_types::{BoardColumn, Status};

const LEFT_W: i32 = 54;
const PANES: [&str; 4] = ["stages", "tests", "artifacts", "log"];

pub fn draw(app: &App, c: &mut Canvas) {
    let (w, h) = (c.w(), c.h());
    let Some(t) = app.cur() else {
        c.boxed(0, 1, w, h - 3, "Run", true, None);
        c.put(2, 3, "no task selected · open a running worker from the dashboard (1)", fg(DIM));
        run_hints(app, c);
        return;
    };
    if app.compact() {
        let title_suffix = format!("  ·  tab → {}", PANES[(app.pane + 1) % 4]);
        let r = (0, 1, w, h - 3);
        match app.pane {
            0 => stages(app, c, t, r, &title_suffix),
            1 => tests(app, c, r, &title_suffix),
            2 => artifacts(app, c, r, &title_suffix),
            _ => log(app, c, t, r, &title_suffix),
        }
    } else {
        let r = h - 3 - 15;
        let ah = if r >= 27 { 13 } else { (r / 2).max(8) };
        let th = r - ah;
        stages(app, c, t, (0, 1, LEFT_W, 15), "");
        tests(app, c, (0, 16, LEFT_W, th), "");
        artifacts(app, c, (0, 16 + th, LEFT_W, ah), "");
        log(app, c, t, (LEFT_W, 1, w - LEFT_W, h - 3), "");
    }
    run_hints(app, c);
}

fn run_hints(app: &App, c: &mut Canvas) {
    hints(
        c,
        app,
        &[
            ("tab", "pane"),
            ("r", "retry"),
            ("s", "stop"),
            ("f", "feedback"),
            ("e", "escalate"),
            ("o", "open artifact"),
            ("L", "logs"),
            ("esc", "back"),
        ],
    );
}

type R = (i32, i32, i32, i32);

/// Stage index for the run view: 0–2 in a pipeline stage, 3 when done, -1 before.
fn stage_index(t: &TaskView) -> i32 {
    match t.column {
        BoardColumn::Building => 0,
        BoardColumn::Qa => 1,
        BoardColumn::Review => 2,
        BoardColumn::Ready | BoardColumn::Done => 3,
        _ => -1,
    }
}

fn stages(app: &App, c: &mut Canvas, t: &TaskView, (x, y, w, h): R, suffix: &str) {
    c.boxed(x, y, w, h, &format!("{} · attempt {}/{}{}", t.key, t.attempt, t.loop_cap, suffix), app.pane == 0, None);
    let x2 = x + 2;
    c.put(x2, y + 1, &trunc(&t.title, (w - 4) as usize), fg(TEXT).bold());
    c.put(x2, y + 2, &trunc(&format!("{} · plan “{}”", t.project, t.plan), (w - 4) as usize), fg(DIM));
    let si = stage_index(t);
    let run = app.data.run.as_ref().filter(|r| r.task == t.key);
    let dur_x = x + 44.min(w - 10);
    for (i, role) in ROLES.iter().enumerate() {
        let yy = y + 4 + i as i32 * 3;
        let ii = i as i32;
        let st = if si == -1 {
            Status::Queued
        } else if ii < si {
            Status::Passed
        } else if ii == si {
            t.status
        } else {
            Status::Queued
        };
        c.put(x2, yy, glyph(app, st), fg(status_color(st)).bold());
        c.put(x2 + 2, yy, role.as_str(), fg(TEXT).bold());
        c.put(x2 + 14, yy, status_label(st), fg(status_color(st)));
        let dur = match st {
            Status::Passed => run.and_then(|r| r.stage_secs[i]).map(fmt_dur).unwrap_or_else(|| "—".into()),
            Status::Running => run
                .and_then(|r| r.stage_started[i])
                .map(|s| fmt_dur((app.now - s).num_seconds().max(0) as u64))
                .unwrap_or_else(|| "—".into()),
            _ => "—".into(),
        };
        c.put(dur_x, yy, &dur, fg(MUTED));
        let wk = &t.workers[i];
        c.put(
            x2 + 2,
            yy + 1,
            &trunc(&format!("{} · {}", harness_slug(&wk.harness), wk.model), (w - 6) as usize),
            fg(DIM),
        );
        if st == Status::Running {
            let bw = (w - 14).min(40);
            c.bar(x2 + 2, yy + 2, bw, t.progress, RUNNING);
            c.put(x2 + 3 + bw, yy + 2, &pct(t.progress), fg(TEXT));
        }
    }
    let ly = y + 13;
    c.put(x2, ly, "loop", fg(DIM));
    for i in 0..3u32 {
        let (g, col) = if i + 1 < t.attempt {
            ("●", FAILED)
        } else if i + 1 == t.attempt {
            (if t.status == Status::Running { "◐" } else { "●" }, status_color(t.status))
        } else {
            ("○", QUEUED)
        };
        c.put(x + 7 + i as i32 * 2, ly, g, fg(col));
    }
    c.put(x + 14, ly, &format!("escalates to a human after {}", t.loop_cap), fg(DIM));
}

fn tests(app: &App, c: &mut Canvas, (x, y, w, h): R, suffix: &str) {
    let run = app.data.run.as_ref().filter(|r| Some(r.task.as_str()) == app.cur().map(|t| t.key.as_str()));
    let (passed, total) = run.map(|r| (r.passed, r.total)).unwrap_or((0, 0));
    c.boxed(x, y, w, h, &format!("Tests · {passed}/{total}{suffix}"), app.pane == 1, None);
    let Some(r) = run.filter(|r| r.total > 0 || !r.tests.is_empty()) else {
        c.put(x + 2, y + 2, &trunc("no QA report yet · tests appear when QA runs", (w - 4) as usize), fg(DIM));
        return;
    };
    let max_rows = (h - 7).max(0) as usize;
    for (i, row) in r.tests.iter().take(max_rows).enumerate() {
        let yy = y + 2 + i as i32;
        c.put(x + 2, yy, glyph(app, row.status), fg(status_color(row.status)));
        let meta_w = display_width(&row.meta) as i32;
        let name_w = (w - 8 - meta_w).max(4) as usize;
        c.put(x + 4, yy, &trunc(&row.name, name_w), fg(if row.status == Status::Failed { FAILED } else { TEXT }));
        c.put(x + w - 2 - meta_w, yy, &row.meta, fg(DIM));
    }
    // Segmented bar: passed / failed / running.
    let by = y + h - 4;
    let cells = (w - 6).max(1);
    if r.total > 0 {
        let share = |n: u32| -> i32 {
            if n == 0 { 0 } else { ((cells as f32 * n as f32 / r.total as f32).round() as i32).max(1) }
        };
        let f = share(r.failed);
        let rn = share(r.running);
        let p = share(r.passed).min(cells - f - rn).max(0);
        let mut bx = x + 2;
        c.put(bx, by, &"█".repeat(p as usize), fg(PASSED));
        bx += p;
        c.put(bx, by, &"█".repeat(f as usize), fg(FAILED));
        bx += f;
        let run_glyph = if app.tick % 8 < 4 { "▒" } else { "░" };
        c.put(bx, by, &run_glyph.repeat(rn as usize), fg(RUNNING));
        bx += rn;
        c.put(bx, by, &"░".repeat((x + 2 + cells - bx).max(0) as usize), fg(FAINT));
    }
    c.put(x + 2, by + 1, &format!("{} passed · {} failed · {} running", r.passed, r.failed, r.running), fg(MUTED));
    if let Some(fl) = &r.failing {
        c.put(x + 2, by + 2, &trunc(&format!("✕ {fl}"), (w - 4) as usize), fg(FAILED));
    }
}

fn artifacts(app: &App, c: &mut Canvas, (x, y, w, h): R, suffix: &str) {
    c.boxed(x, y, w, h, &format!("Artifacts · qa{suffix}"), app.pane == 2, None);
    let run = app.data.run.as_ref().filter(|r| Some(r.task.as_str()) == app.cur().map(|t| t.key.as_str()));
    let arts = run.map(|r| r.artifacts.as_slice()).unwrap_or(&[]);
    if arts.is_empty() {
        c.put(
            x + 2,
            y + 2,
            &trunc("no artifacts yet · QA captures screenshots, video and logs", (w - 4) as usize),
            fg(DIM),
        );
        return;
    }
    let max_rows = (h - 8).max(1) as usize;
    let off = app.asel.saturating_sub(max_rows - 1);
    for (i, a) in arts.iter().enumerate().skip(off).take(max_rows) {
        let yy = y + 2 + (i - off) as i32;
        let on = app.pane == 2 && i == app.asel;
        let b = if on { Some(SEL) } else { None };
        if on {
            c.fill(x + 1, yy, w - 2, 1, SEL);
        }
        c.put(x + 2, yy, &a.glyph, fg(if a.glyph == "±" { ACCENT } else { MUTED }).on_opt(b));
        let meta_w = display_width(&a.meta) as i32;
        c.put(x + 4, yy, &trunc(&a.name, (w - 8 - meta_w).max(4) as usize), fg(TEXT).on_opt(b).bold_if(on));
        c.put(x + w - 2 - meta_w, yy, &a.meta, fg(DIM).on_opt(b));
    }
    let sel = &arts[app.asel.min(arts.len() - 1)];
    let py = y + h - 5;
    c.put(x + 2, py, &trunc(&format!("preview stub · {}", sel.name), (w - 4) as usize), fg(MUTED));
    c.fill(x + 2, py + 1, w - 4, 2, RAISED);
    c.put(x + 4, py + 1, &trunc(&format!("{}  {}", sel.glyph, sel.meta), (w - 8) as usize), fg(DIM).on(RAISED));
    c.put(x + 4, py + 2, &trunc("o opens in Preview / mpv · no inline images", (w - 8) as usize), fg(DIM).on(RAISED));
}

fn log(app: &App, c: &mut Canvas, t: &TaskView, (x, y, w, h): R, suffix: &str) {
    let si = stage_index(t).clamp(0, 2) as usize;
    let role = ROLES[si];
    c.boxed(x, y, w, h, &format!("Log · {} · follow{suffix}", role.as_str()), app.pane == 3, None);
    let lines: Vec<_> =
        app.data.logs.get(&t.key).map(|v| v.iter().filter(|l| l.role == role).collect()).unwrap_or_default();
    let rows = (h - 4).max(0) as usize;
    let start = lines.len().saturating_sub(rows);
    let vis = &lines[start..];
    let msg_w = (w - 22).max(4) as usize;
    for (i, l) in vis.iter().enumerate() {
        let yy = y + 1 + i as i32 + (rows - vis.len()) as i32;
        c.put(x + 2, yy, &l.ts, fg(DIM));
        let fail = l.level == kiln_types::event::LogLevel::Fail;
        c.put(x + 12, yy, l.level.as_str(), fg(level_color(l.level)).bold_if(fail));
        c.put(x + 18, yy, &trunc(&l.msg, msg_w), fg(if fail { FAILED } else { TEXT }));
    }
    if lines.is_empty() {
        c.put(x + 2, y + 2, "waiting for log lines…", fg(DIM));
    }
    let fy = y + h - 2;
    if t.status == Status::Running {
        c.put(x + 2, fy, &format!("{} streaming", app.spinner()), fg(RUNNING));
    } else {
        c.put(x + 2, fy, "● complete", fg(MUTED));
    }
    c.put(
        x + 16,
        fy,
        &trunc(
            &format!("{} lines · L full logs · tab to switch pane", thousands(lines.len())),
            (w - 18).max(0) as usize,
        ),
        fg(DIM),
    );
}
