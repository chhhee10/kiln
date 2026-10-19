//! Snapshot-style render tests: every screen at 160×45 (the prototype's grid,
//! asserting strings, glyphs and colors at the design's coordinates) and the
//! key screens at 80×24.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kiln_tui::app::{App, Mode, Msg, Overlay, Screen};
use kiln_tui::model::{DataEvent, FlashMsg, LogLineView, PairView};
use kiln_tui::testing::*;
use kiln_tui::theme::{self, *};
use kiln_tui::view::render_buffer;
use kiln_types::WorkerRole;
use kiln_types::event::LogLevel;
use ratatui::style::Color;

fn app(screen: Screen) -> App {
    demo_app(screen, 160, 45)
}

fn key(app: &mut App, c: char) {
    let m = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
    app.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), m)));
}

fn code(app: &mut App, k: KeyCode) {
    app.update(Msg::Key(KeyEvent::new(k, KeyModifiers::NONE)));
}

#[track_caller]
fn at(buf: &ratatui::buffer::Buffer, x: u16, y: u16, s: &str) {
    let got = text_at(buf, x, y, s.chars().count() as u16);
    assert_eq!(got, s, "at ({x},{y})\nrow: {}", row(buf, y));
}

// ---------------------------------------------------------------- shared chrome

#[test]
fn header_tabs_and_clock() {
    let b = render_buffer(&app(Screen::Dash), 160, 45);
    at(&b, 1, 0, "◆");
    assert_eq!(fg_at(&b, 1, 0), ACCENT);
    assert!(bold_at(&b, 1, 0));
    at(&b, 3, 0, "Kiln");
    // Tabs start at x=15; the active one is ink on accent.
    at(&b, 15, 0, " 1 Dashboard ");
    assert_eq!(bg_at(&b, 16, 0), ACCENT);
    assert_eq!(fg_at(&b, 16, 0), ON_ACCENT);
    at(&b, 29, 0, " 2 Run ");
    assert_eq!(bg_at(&b, 30, 0), PANEL);
    assert_eq!(fg_at(&b, 30, 0), MUTED);
    at(&b, 37, 0, " 3 Plans ");
    at(&b, 47, 0, " 4 Planner ");
    at(&b, 59, 0, " 5 Logs ");
    at(&b, 68, 0, " 6 Credentials ");
    // Right side: user, host, green dot, clock.
    let rt = "alex@company.dev  mac-mini ● 12:04:31 ";
    at(&b, 160 - rt.chars().count() as u16, 0, rt);
    assert_eq!(fg_at(&b, 160 - rt.chars().count() as u16 + 27, 0), PASSED);
    assert_eq!(bg_at(&b, 100, 0), PANEL);
}

#[test]
fn hint_bar_mode_chip_and_pairs() {
    let b = render_buffer(&app(Screen::Dash), 160, 45);
    at(&b, 0, 44, " NORMAL ");
    assert_eq!(bg_at(&b, 1, 44), ACCENT);
    assert_eq!(fg_at(&b, 1, 44), ON_ACCENT);
    assert!(bold_at(&b, 1, 44));
    at(&b, 10, 44, "j/k move");
    assert_eq!(fg_at(&b, 10, 44), ACCENT);
    assert_eq!(fg_at(&b, 14, 44), MUTED);
    at(&b, 21, 44, "⏎ open run");
    assert!(row(&b, 44).contains("q sign out"));
    assert!(!row(&b, 44).contains("more"));
    assert_eq!(bg_at(&b, 150, 44), PANEL);
}

#[test]
fn status_line_context_and_flash() {
    let mut a = app(Screen::Dash);
    let b = render_buffer(&a, 160, 45);
    at(&b, 1, 43, "mac-mini · 4 workers running · 3 queued · last sync just now");
    assert_eq!(fg_at(&b, 1, 43), DIM);
    a.update(Msg::Data(DataEvent::Flash(FlashMsg::err("✕ boom"))));
    let b = render_buffer(&a, 160, 45);
    at(&b, 1, 43, "✕ boom");
    assert_eq!(fg_at(&b, 1, 43), FAILED);
    for _ in 0..27 {
        a.update(Msg::Tick);
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 1, 43, "mac-mini");
}

// ---------------------------------------------------------------- dashboard

#[test]
fn dashboard_160x45() {
    let b = render_buffer(&app(Screen::Dash), 160, 45);
    // Board box.
    at(&b, 0, 1, "╭─ Board · all projects ");
    assert_eq!(fg_at(&b, 0, 1), LINE);
    assert_eq!(fg_at(&b, 3, 1), MUTED);
    at(&b, 103, 1, "╮");
    at(&b, 2, 3, "Idea 2");
    assert!(bold_at(&b, 2, 3));
    at(&b, 16, 3, "Planned 3");
    at(&b, 30, 3, "Building 2");
    at(&b, 44, 3, "QA 2");
    at(&b, 58, 3, "Review 2");
    at(&b, 72, 3, "Ready 2");
    at(&b, 86, 3, "Done 3");
    at(&b, 2, 4, "────────────");
    assert_eq!(fg_at(&b, 2, 4), LINE);
    at(&b, 46, 5, "FP-142");
    assert_eq!(fg_at(&b, 44, 5), RUNNING);
    at(&b, 44, 6, "✕ FP-139");
    assert_eq!(fg_at(&b, 44, 6), FAILED);
    at(&b, 58, 6, "! FP-136");
    assert_eq!(fg_at(&b, 58, 6), HUMAN);
    // Done ids are dim.
    assert_eq!(fg_at(&b, 88, 5), DIM);
    at(&b, 2, 14, "16 tasks · 4 running · 1 failed · 1 needs human · 2 ready to review");
    assert_eq!(fg_at(&b, 11, 14), FAINT);

    // Machine.
    at(&b, 104, 1, "╭─ Machine · mac-mini ");
    at(&b, 106, 3, "cpu");
    let spark = text_at(&b, 112, 3, 34);
    assert!(spark.chars().all(|c| "▁▂▃▄▅▆▇█".contains(c)), "{spark}");
    assert_eq!(fg_at(&b, 112, 3), RUNNING);
    at(&b, 106, 5, "ram");
    at(&b, 148, 5, "19.1G");
    at(&b, 106, 7, "ctr   ■ ■ ■ ■ ■ □ □ □   5/8 slots");
    assert_eq!(fg_at(&b, 122, 7), FAINT);
    at(&b, 112, 9, "3 waiting · est. ~4m");
    at(&b, 112, 11, "builder 2   qa 1   reviewer 1");
    at(&b, 106, 13, "up 18d · 100.84.12.7 · M4 Pro 12c · 32 GB");

    // Running workers: focused box.
    at(&b, 0, 16, "╭─ Running workers ");
    assert_eq!(fg_at(&b, 0, 16), ACCENT);
    assert!(bold_at(&b, 3, 16));
    at(&b, 4, 17, "ID");
    at(&b, 12, 17, "TASK");
    at(&b, 48, 17, "STAGE");
    at(&b, 59, 17, "HARNESS · MODEL");
    at(&b, 82, 17, "PROGRESS");
    at(&b, 4, 18, "FP-145");
    at(&b, 48, 18, "builder");
    at(&b, 59, 18, "claude · claude-sonne…");
    // dsel = 2 → FP-142 selected.
    at(&b, 1, 20, "▌");
    assert_eq!(fg_at(&b, 1, 20), ACCENT);
    assert_eq!(bg_at(&b, 1, 20), SEL);
    assert_eq!(bg_at(&b, 50, 20), SEL);
    at(&b, 4, 20, "FP-142");
    assert!(bold_at(&b, 4, 20));
    at(&b, 48, 20, "qa");
    at(&b, 59, 20, "codex · gpt-5");
    at(&b, 82, 20, "██████▋░░░░░░░");
    at(&b, 97, 20, " 48%");
    at(&b, 2, 30, "⏎ open run · j/k move · 4 of 8 slots in use");

    // Inbox.
    at(&b, 104, 16, "╭─ Inbox · needs you ");
    at(&b, 106, 18, "! FP-136 SSO login polish — needs you");
    assert_eq!(fg_at(&b, 106, 18), HUMAN);
    at(&b, 108, 19, "escalated after 3 loops · 14m");
    assert_eq!(fg_at(&b, 108, 19), DIM);
    at(&b, 106, 28, "◑ 2 memories await Company approval");

    // Activity.
    at(&b, 0, 32, "╭─ Activity ");
    at(&b, 159, 32, "╮");
    at(&b, 12, 33, "bld");
    at(&b, 18, 33, "◐ FP-145 builder committed 3 files");
    assert_eq!(fg_at(&b, 20, 33), TEXT);
    assert_eq!(fg_at(&b, 20, 34), MUTED);
    at(&b, 0, 42, "╰");
}

#[test]
fn dashboard_80x24_collapses() {
    let mut a = demo_app(Screen::Dash, 80, 24);
    let b = render_buffer(&a, 80, 24);
    at(&b, 1, 0, "◆");
    assert!(row(&b, 0).contains(" 1 Dash "));
    assert!(row(&b, 0).contains("12:04:31"));
    // Board: counts only.
    at(&b, 2, 2, "Idea 2  Planned 3  Building 2  QA 2  Review 2  Ready 2  Done 3");
    at(&b, 2, 3, "16 tasks · 4 running");
    assert!(find(&b, "FP-151").is_none());
    // Workers table still there.
    at(&b, 0, 5, "╭─ Running workers ");
    assert!(find(&b, "FP-142").is_some());
    // Collapsed right panes, Machine first; tab cycles.
    assert!(row(&b, 14).contains("Machine · Inbox · Activity"));
    assert_eq!(fg_at(&b, 3, 14), ACCENT);
    at(&b, 2, 15, "cpu");
    code(&mut a, KeyCode::Tab);
    let b = render_buffer(&a, 80, 24);
    assert!(find(&b, "FP-136 SSO login polish").is_some());
    code(&mut a, KeyCode::Tab);
    let b = render_buffer(&a, 80, 24);
    assert!(find(&b, "builder committed").is_some());
    // Hint bar truncates with "? more".
    let hint = row(&b, 23);
    assert!(hint.trim_end().ends_with("? more"), "{hint}");
    assert!(!hint.contains("sign out"));
}

// ---------------------------------------------------------------- run

#[test]
fn run_160x45() {
    let a = app(Screen::Run);
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 1, "╭─ FP-142 · attempt 2/3 ");
    assert_eq!(fg_at(&b, 0, 1), LINE); // log pane is focused by default
    at(&b, 2, 2, "Add retry policy to SDK client");
    assert!(bold_at(&b, 2, 2));
    at(&b, 2, 3, "CLI/SDK · plan “Retry & backoff”");
    at(&b, 2, 5, "✓ builder     passed");
    assert_eq!(fg_at(&b, 2, 5), PASSED);
    at(&b, 44, 5, "4m12s");
    at(&b, 4, 6, "claude-agent-sdk · claude-sonnet-4.5");
    assert_eq!(fg_at(&b, 2, 8), RUNNING);
    at(&b, 4, 8, "qa          running");
    at(&b, 4, 9, "codex · gpt-5");
    at(&b, 4, 10, "███████████████████▏");
    at(&b, 45, 10, " 48%");
    at(&b, 2, 11, "○ reviewer    queued");
    at(&b, 44, 11, "—");
    at(&b, 2, 14, "loop ● ◐ ○  escalates to a human after 3");
    assert_eq!(fg_at(&b, 7, 14), FAILED);
    assert_eq!(fg_at(&b, 9, 14), RUNNING);
    assert_eq!(fg_at(&b, 11, 14), QUEUED);

    at(&b, 0, 16, "╭─ Tests · 41/43 ");
    at(&b, 2, 18, "✓ client/retry.test.ts");
    at(&b, 48, 18, "0.8s");
    at(&b, 4, 23, "e2e/settings.spec.ts");
    assert_eq!(fg_at(&b, 4, 23), FAILED);
    assert_eq!(fg_at(&b, 2, 26), PASSED);
    assert_eq!(fg_at(&b, 47, 26), PASSED);
    assert_eq!(fg_at(&b, 48, 26), FAILED);
    assert_eq!(fg_at(&b, 49, 26), RUNNING);
    at(&b, 2, 27, "41 passed · 1 failed · 1 running");
    at(&b, 2, 28, "✕ settings.spec:42 expected toast “Saved”");

    at(&b, 0, 30, "╭─ Artifacts · qa ");
    at(&b, 2, 32, "▣ 01-init-prompt.png");
    at(&b, 31, 32, "1440×900 png · 412 KB");
    at(&b, 2, 36, "± diff");
    assert_eq!(fg_at(&b, 2, 36), ACCENT);
    at(&b, 2, 38, "preview stub · 02-retry-banner.png");
    assert_eq!(bg_at(&b, 2, 39), RAISED);
    at(&b, 4, 40, "o opens in Preview / mpv · no inline images");

    at(&b, 54, 1, "╭─ Log · qa · follow ");
    assert_eq!(fg_at(&b, 54, 1), ACCENT);
    at(&b, 159, 1, "╮");
    at(&b, 66, 39, "FAIL");
    assert_eq!(fg_at(&b, 66, 39), FAILED);
    assert!(bold_at(&b, 66, 39));
    at(&b, 72, 39, "e2e/settings.spec.ts");
    assert_eq!(fg_at(&b, 72, 39), FAILED);
    assert_eq!(fg_at(&b, 56, 41), RUNNING);
    at(&b, 58, 41, "streaming");
    at(&b, 70, 41, "24 lines · L full logs · tab to switch pane");
    at(&b, 1, 43, "FP-142 · Add retry policy to SDK client · draft PR #418");
    at(&b, 10, 44, "tab pane");
    assert!(row(&b, 44).contains("o open artifact"));
}

#[test]
fn run_pane_focus_and_artifact_selection() {
    let mut a = app(Screen::Run);
    code(&mut a, KeyCode::Tab); // → stages
    let b = render_buffer(&a, 160, 45);
    assert_eq!(fg_at(&b, 0, 1), ACCENT);
    assert_eq!(fg_at(&b, 54, 1), LINE);
    code(&mut a, KeyCode::Tab);
    code(&mut a, KeyCode::Tab); // → artifacts
    let b = render_buffer(&a, 160, 45);
    assert_eq!(fg_at(&b, 0, 30), ACCENT);
    assert_eq!(bg_at(&b, 4, 33), SEL);
    assert!(bold_at(&b, 4, 33));
    key(&mut a, 'j');
    let b = render_buffer(&a, 160, 45);
    assert_eq!(bg_at(&b, 4, 34), SEL);
    at(&b, 2, 38, "preview stub · session.webm");
}

#[test]
fn run_grows_log_pane_above_160() {
    let a = demo_app(Screen::Run, 200, 45);
    let b = render_buffer(&a, 200, 45);
    at(&b, 54, 1, "╭─ Log");
    at(&b, 199, 1, "╮");
    at(&b, 53, 1, "╮"); // left column stays 54 wide
}

#[test]
fn run_80x24_single_pane() {
    let mut a = demo_app(Screen::Run, 80, 24);
    let b = render_buffer(&a, 80, 24);
    assert!(row(&b, 1).contains("Log · qa · follow  ·  tab → stages"));
    at(&b, 79, 1, "╮");
    code(&mut a, KeyCode::Tab);
    let b = render_buffer(&a, 80, 24);
    assert!(row(&b, 1).contains("FP-142 · attempt 2/3"));
    assert!(find(&b, "escalates to a human after 3").is_some());
    assert!(row(&b, 23).trim_end().ends_with("? more"));
}

// ---------------------------------------------------------------- plans

#[test]
fn plans_160x45() {
    let mut a = app(Screen::Plans);
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 1, "╭─ Plans ");
    at(&b, 1, 3, "▌");
    at(&b, 1, 4, "▌");
    at(&b, 3, 3, "◇ Retry & backoff for SDK client");
    assert_eq!(fg_at(&b, 3, 3), ACCENT);
    assert!(bold_at(&b, 5, 3));
    at(&b, 5, 4, "CLI/SDK · 3 tasks · needs approval");
    assert_eq!(fg_at(&b, 25, 4), ACCENT);
    at(&b, 3, 6, "◐ Usage chart on billing page");
    at(&b, 3, 40, "n new idea → planner (4)");

    at(&b, 46, 1, "╭─ Plan · draft v3 ");
    assert_eq!(fg_at(&b, 46, 1), ACCENT);
    at(&b, 48, 3, "Retry & backoff for SDK client");
    at(&b, 48, 4, "CLI/SDK · drafted by Planner with Alex · 3 memories in context");
    at(&b, 47, 7, "▌▾ T1  Retry policy core with backoff + jitter");
    assert_eq!(bg_at(&b, 100, 7), SEL);
    at(&b, 52, 8, "acceptance criteria");
    at(&b, 54, 9, "○ Retries 5xx and network errors up to maxRetries");
    assert_eq!(fg_at(&b, 54, 9), FAINT);
    at(&b, 52, 12, "workers");
    at(&b, 66, 12, "harness");
    assert_eq!(fg_at(&b, 66, 12), FAINT);
    at(&b, 88, 12, "model");
    at(&b, 54, 13, "builder");
    at(&b, 66, 13, "Claude Agent SDK");
    at(&b, 88, 13, "claude-sonnet-4.5");
    at(&b, 48, 26, "▸ T3  Docs page: configuring retries  2 criteria · claude / claude / gpt");
    at(&b, 48, 40, "3 tasks · up to 27 worker runs · loop max 3 → escalate");
    at(&b, 134, 40, " a  approve & dispatch ");
    assert_eq!(bg_at(&b, 134, 40), ACCENT);
    at(&b, 1, 43, "plan “Retry & backoff for SDK client” · draft v3 · needs approval");

    // Move to the builder worker row: edit hint appears at x=112.
    for _ in 0..4 {
        key(&mut a, 'j');
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 47, 13, "▌");
    at(&b, 112, 13, "‹ h/l model · H/L harness ›");
    assert_eq!(fg_at(&b, 112, 13), ACCENT);
    key(&mut a, 'l');
    let b = render_buffer(&a, 160, 45);
    at(&b, 88, 13, "claude-opus-4.1");
    key(&mut a, 'L');
    let b = render_buffer(&a, 160, 45);
    at(&b, 66, 13, "Codex");
}

#[test]
fn approve_confirm_dialog_64x9() {
    let mut a = app(Screen::Plans);
    key(&mut a, 'a');
    let b = render_buffer(&a, 160, 45);
    at(&b, 48, 16, "╭─ Approve & dispatch ");
    assert_eq!(fg_at(&b, 48, 16), ACCENT);
    at(&b, 111, 16, "╮");
    at(&b, 48, 24, "╰");
    at(&b, 51, 18, "Dispatch 3 tasks to workers?");
    assert!(bold_at(&b, 51, 18));
    assert_eq!(bg_at(&b, 51, 18), RAISED);
    // The prototype's 61-char line overruns the 64-wide box border; it is
    // truncated to keep the border intact.
    at(&b, 51, 19, "builder → qa → reviewer per task · loops max 3, then esca…");
    at(&b, 111, 19, "│");
    at(&b, 51, 21, " y  dispatch ");
    assert_eq!(bg_at(&b, 51, 21), ACCENT);
    at(&b, 66, 21, " n  cancel ");
    assert_eq!(bg_at(&b, 66, 21), FIELD);
}

// ---------------------------------------------------------------- planner chat

#[test]
fn chat_160x45() {
    let mut a = app(Screen::Chat);
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 1, "╭─ Threads ");
    at(&b, 1, 3, "▌ Retry & backoff for SDK client");
    at(&b, 3, 4, "needs approval");
    assert_eq!(fg_at(&b, 3, 4), ACCENT);
    at(&b, 40, 1, "╭─ Planner · Retry & backoff for SDK client · opus-4.1 ");
    assert_eq!(fg_at(&b, 40, 1), ACCENT);
    let (_, y) = find(&b, "◆ planner").unwrap();
    assert_eq!(fg_at(&b, 44, y), ACCENT);
    assert!(bold_at(&b, 44, y));
    let (yx, yy) = find(&b, "you").filter(|(x, _)| *x == 44).unwrap();
    assert_eq!(fg_at(&b, yx, yy), MUTED);
    at(&b, 44, 35, "Codex for a second opinion. Edit anything there, then press a to approve and dispatch.");
    at(&b, 40, 39, "╭─ Message ");
    assert_eq!(fg_at(&b, 40, 39), LINE);
    at(&b, 42, 40, "›");
    assert_eq!(fg_at(&b, 42, 40), ACCENT);
    at(&b, 44, 40, "press i to write · enter sends · esc returns to normal");
    at(&b, 42, 41, "3 memories in context · plan auto-updates · voice input on Android");
    at(&b, 1, 43, "planner thread · 4 messages");

    // INSERT mode: composer focused, chip switches.
    key(&mut a, 'i');
    for c in "hi".chars() {
        key(&mut a, c);
    }
    let b = render_buffer(&a, 160, 45);
    assert_eq!(fg_at(&b, 40, 39), ACCENT);
    assert_eq!(fg_at(&b, 40, 1), LINE);
    at(&b, 44, 40, "hi");
    at(&b, 0, 44, " INSERT ");
    assert_eq!(bg_at(&b, 1, 44), RUNNING);
    at(&b, 10, 44, "⏎ send");

    // Send → pending spinner line.
    code(&mut a, KeyCode::Enter);
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "reading company brain · drafting changes…").is_some());
}

// ---------------------------------------------------------------- logs

#[test]
fn logs_160x45_follow_pause_filter() {
    let mut a = app(Screen::Logs);
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 1, "╭─ Logs · FP-142 · tail -f ");
    assert_eq!(fg_at(&b, 0, 1), ACCENT);
    at(&b, 2, 2, "  builder   [ qa ]   reviewer  ");
    assert_eq!(fg_at(&b, 14, 2), ACCENT);
    assert!(bold_at(&b, 14, 2));
    assert_eq!(fg_at(&b, 4, 2), MUTED);
    at(&b, 62, 2, "following");
    assert_eq!(fg_at(&b, 60, 2), RUNNING);
    at(&b, 110, 2, "/ filter");
    assert_eq!(fg_at(&b, 110, 2), ACCENT);
    assert_eq!(fg_at(&b, 112, 2), DIM);
    at(&b, 1, 3, "────");
    at(&b, 12, 4, "INFO");
    assert_eq!(fg_at(&b, 12, 4), DIM);
    at(&b, 18, 4, "[qa]");
    assert_eq!(fg_at(&b, 18, 4), FAINT);
    at(&b, 30, 4, "booting app container fp-cli:qa");
    at(&b, 2, 41, "24 of 24 lines · ~/.local/share/kiln/runs/FP-142/qa.log");

    // Pause; a new line arrives but the view stays.
    key(&mut a, 'f');
    a.update(Msg::Data(DataEvent::Log {
        task: "FP-142".into(),
        line: LogLineView {
            idx: 0,
            ts: "12:05:00".into(),
            role: WorkerRole::Qa,
            level: LogLevel::Pass,
            msg: "NEW LINE".into(),
        },
    }));
    let b = render_buffer(&a, 160, 45);
    at(&b, 60, 2, "❚❚ paused · 1 new lines · f to follow");
    assert_eq!(fg_at(&b, 60, 2), HUMAN);
    assert!(find(&b, "NEW LINE").is_none());
    key(&mut a, 'G');
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "NEW LINE").is_some());

    // Filter.
    key(&mut a, '/');
    for c in "fail".chars() {
        key(&mut a, c);
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 44, " FILTER ");
    assert_eq!(bg_at(&b, 1, 44), RUNNING);
    at(&b, 112, 2, "fail");
    at(&b, 12, 4, "FAIL");
    assert!(row(&b, 41).contains("match"));
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Normal);
    for c in "zzz".chars() {
        a.logs.filter.push(c);
    }
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "no lines match “failzzz”").is_some());
}

#[test]
fn logs_80x24() {
    let a = demo_app(Screen::Logs, 80, 24);
    let b = render_buffer(&a, 80, 24);
    assert!(row(&b, 2).contains("[ qa ]"));
    assert!(row(&b, 2).contains("following"));
    assert!(row(&b, 2).contains("/ filter"));
    assert!(row(&b, 20).contains("lines · ~/.local/share/kiln/runs/FP-142/qa.log"));
}

// ---------------------------------------------------------------- credentials

#[test]
fn credentials_160x45() {
    let mut a = app(Screen::Creds);
    let b = render_buffer(&a, 160, 45);
    at(&b, 0, 1, "╭─ Credentials · per-user vault on mac-mini ");
    assert_eq!(fg_at(&b, 0, 1), ACCENT);
    at(&b, 3, 3, "Anthropic");
    at(&b, 74, 3, "API key");
    at(&b, 3, 4, "sk-ant-••••••••••4f2a");
    at(&b, 3, 5, "✓ valid · used 2h ago");
    assert_eq!(fg_at(&b, 3, 5), PASSED);
    at(&b, 5, 13, "expires in 6 days");
    assert_eq!(fg_at(&b, 5, 13), HUMAN);
    // csel = 3 (LiteLLM).
    at(&b, 1, 15, "▌ LiteLLM proxy");
    at(&b, 1, 17, "▌");
    assert_eq!(bg_at(&b, 40, 16), SEL);
    at(&b, 72, 15, "URL + key");
    at(&b, 3, 16, "https://litellm.tailnet:4000  ·  sk-llm-••••••••••c0de");
    at(&b, 3, 20, "setup checklist");
    at(&b, 3, 23, "! GitHub token expires in 6 days · rotate soon");
    assert_eq!(fg_at(&b, 5, 23), HUMAN);
    at(&b, 3, 41, "keys belong to you · workers you dispatch use your keys");

    at(&b, 84, 1, "╭─ Edit · LiteLLM proxy ");
    assert_eq!(fg_at(&b, 84, 1), LINE);
    at(&b, 86, 3, "provider");
    at(&b, 86, 4, "LiteLLM proxy");
    at(&b, 86, 9, "base url");
    assert_eq!(fg_at(&b, 86, 9), ACCENT);
    assert_eq!(bg_at(&b, 86, 10), FIELD);
    at(&b, 87, 10, "https://litellm.tailnet:4000");
    at(&b, 86, 13, "api key  · secret");
    assert_eq!(bg_at(&b, 86, 14), RAISED);
    at(&b, 86, 18, "✓ reachable · 34ms");
    at(&b, 86, 20, " t  test connection ");
    assert_eq!(bg_at(&b, 86, 20), ACCENT);
    at(&b, 108, 20, " n  add credential ");
    at(&b, 86, 39, "stored encrypted with your login key · never sent to the planner");
    at(&b, 86, 40, "litellm: containers reach the proxy over the tailnet");
    at(&b, 1, 43, "4 credentials · 1 needs attention");

    // Edit the secret: only the last typed char is visible.
    code(&mut a, KeyCode::Tab);
    code(&mut a, KeyCode::Enter);
    key(&mut a, 'Z');
    let b = render_buffer(&a, 160, 45);
    assert_eq!(fg_at(&b, 84, 1), ACCENT);
    at(&b, 87, 14, "•••••••••••••Z");
    at(&b, 0, 44, " INSERT ");

    // Testing spinner.
    code(&mut a, KeyCode::Esc);
    key(&mut a, 't');
    let b = render_buffer(&a, 160, 45);
    assert!(row(&b, 18).contains("testing connection from a worker container…"));
    assert_eq!(fg_at(&b, 86, 18), RUNNING);
}

#[test]
fn credentials_80x24() {
    let mut a = demo_app(Screen::Creds, 80, 24);
    let b = render_buffer(&a, 80, 24);
    assert!(row(&b, 1).contains("Credentials · per-user vault"));
    assert!(find(&b, "LiteLLM proxy").is_some());
    code(&mut a, KeyCode::Enter);
    let b = render_buffer(&a, 80, 24);
    assert!(row(&b, 1).contains("Edit · LiteLLM proxy"));
}

// ---------------------------------------------------------------- login / setup

#[test]
fn login_160x45() {
    let mut a = app(Screen::Login);
    a.login.field = 1;
    let b = render_buffer(&a, 160, 45);
    at(&b, 2, 1, "·");
    assert_eq!(fg_at(&b, 2, 1), DOTS);
    at(&b, 1, 0, "◆ Kiln");
    at(&b, 130, 0, "mac-mini.tailnet · 12:04:31 ");
    at(&b, 48, 10, "╭─ Sign in ");
    assert_eq!(fg_at(&b, 48, 10), ACCENT);
    assert!(bold_at(&b, 51, 10));
    at(&b, 111, 10, "╮");
    at(&b, 48, 31, "╰");
    at(&b, 52, 12, "◆ Kiln");
    at(&b, 52, 13, "agentic development platform");
    at(&b, 52, 15, "email");
    assert_eq!(fg_at(&b, 52, 15), MUTED);
    assert_eq!(bg_at(&b, 52, 16), FIELD);
    assert_eq!(bg_at(&b, 107, 16), FIELD);
    at(&b, 53, 16, "alex@company.dev");
    at(&b, 52, 18, "password");
    assert_eq!(fg_at(&b, 52, 18), ACCENT);
    assert!(bold_at(&b, 52, 18));
    at(&b, 55, 19, "type anything");
    at(&b, 52, 23, " ⏎  sign in ");
    assert_eq!(bg_at(&b, 52, 23), ACCENT);
    at(&b, 68, 23, "tab switch field");
    at(&b, 52, 26, "● mac-mini.tailnet · reachable · 18ms");
    assert_eq!(fg_at(&b, 52, 26), PASSED);
    at(&b, 52, 27, "accounts: @company.dev allowlist, or created by an admin");
    at(&b, 52, 28, "session → ~/.config/kiln/session.toml");
    at(&b, 0, 44, " INSERT ");
    assert_eq!(bg_at(&b, 1, 44), RUNNING);
    at(&b, 10, 44, "tab next field");

    // Password is masked, errors in red.
    key(&mut a, 'x');
    key(&mut a, 'y');
    a.update(Msg::Data(DataEvent::LoginError { message: "✕ password required".into(), password_field: true }));
    let b = render_buffer(&a, 160, 45);
    at(&b, 53, 19, "••");
    at(&b, 52, 21, "✕ password required");
    assert_eq!(fg_at(&b, 52, 21), FAILED);
}

#[test]
fn login_80x24() {
    let a = demo_app(Screen::Login, 80, 24);
    let b = render_buffer(&a, 80, 24);
    at(&b, 8, 1, "╭─ Sign in ");
    at(&b, 8, 22, "╰");
    at(&b, 12, 3, "◆ Kiln");
    at(&b, 0, 23, " INSERT ");
}

#[test]
fn setup_wizard_160x45_and_80x24() {
    let mut a = app(Screen::Setup);
    let b = render_buffer(&a, 160, 45);
    let (bx, by) = find(&b, "╭─ First-run setup").unwrap();
    assert_eq!(bx, 48);
    assert_eq!(fg_at(&b, bx, by), ACCENT);
    assert!(find(&b, "● admin account   ○ allowed domain   ○ first project").is_some());
    assert!(find(&b, "your name").is_some());
    assert!(find(&b, "confirm password").is_some());
    assert!(find(&b, " ⏎  continue ").is_some());
    at(&b, 0, 44, " INSERT ");
    // Step 2.
    for (i, v) in ["Alex Rao", "alex@company.dev", "correct-horse", "correct-horse"].iter().enumerate() {
        a.setup.fields[0][i] = v.to_string();
    }
    code(&mut a, KeyCode::Enter);
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "✓ admin account   ● allowed domain").is_some());
    assert!(find(&b, "company.dev").is_some());
    assert!(find(&b, "teammates with an @company.dev email can self-register").is_some());

    let a = demo_app(Screen::Setup, 80, 24);
    let b = render_buffer(&a, 80, 24);
    assert!(find(&b, "First-run setup").is_some());
    assert!(find(&b, " ⏎  continue ").is_some());
}

// ---------------------------------------------------------------- overlays

#[test]
fn palette_72x22() {
    let mut a = app(Screen::Dash);
    key(&mut a, ':');
    let b = render_buffer(&a, 160, 45);
    at(&b, 44, 6, "╭─ Command palette ");
    assert_eq!(fg_at(&b, 44, 6), ACCENT);
    at(&b, 115, 6, "╮");
    at(&b, 44, 27, "╰");
    at(&b, 46, 8, "›");
    at(&b, 49, 8, "type a command, screen or task id");
    at(&b, 105, 8, "12 cmds");
    at(&b, 45, 9, "──────");
    at(&b, 46, 10, "▸ go to    Dashboard");
    assert_eq!(bg_at(&b, 46, 10), SEL);
    assert!(bold_at(&b, 57, 10));
    at(&b, 112, 10, "1");
    assert_eq!(fg_at(&b, 112, 10), ACCENT);
    at(&b, 57, 11, "Run");
    assert_eq!(fg_at(&b, 57, 11), MUTED);
    assert_eq!(bg_at(&b, 57, 11), RAISED);
    at(&b, 46, 26, "↑↓ ctrl-n/p select · ⏎ run · esc close");
    at(&b, 0, 44, " PALETTE ");
    assert_eq!(bg_at(&b, 1, 44), ACCENT);
    at(&b, 11, 44, "type filter");
    // Filter.
    for c in "escal".chars() {
        key(&mut a, c);
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 48, 8, "escal");
    // Substring matches rank first; fuzzy subsequence matches follow.
    at(&b, 57, 10, "Escalate FP-142");
    assert!(text_at(&b, 105, 8, 7).ends_with("cmds "));
    for c in "qqqq".chars() {
        key(&mut a, c);
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 48, 11, "no matches");
}

#[test]
fn help_96x33_six_groups() {
    let mut a = app(Screen::Dash);
    key(&mut a, '?');
    let b = render_buffer(&a, 160, 45);
    at(&b, 32, 5, "╭─ Keybindings ");
    at(&b, 127, 5, "╮");
    at(&b, 32, 37, "╰");
    for (x, y, g) in [
        (36, 7, "global"),
        (82, 7, "lists"),
        (36, 17, "run"),
        (82, 17, "plans"),
        (36, 27, "logs"),
        (82, 27, "credentials"),
    ] {
        at(&b, x, y, g);
        assert_eq!(fg_at(&b, x, y), ACCENT);
        assert!(bold_at(&b, x, y));
    }
    at(&b, 36, 8, "1-6");
    at(&b, 50, 8, "switch screen");
    assert_eq!(fg_at(&b, 50, 8), MUTED);
    at(&b, 36, 9, ": · ctrl-k");
    at(&b, 82, 18, "space");
    at(&b, 96, 18, "expand task");
    at(&b, 36, 36, "press ? or esc to close");

    let mut a = demo_app(Screen::Dash, 80, 24);
    key(&mut a, '?');
    let b = render_buffer(&a, 80, 24);
    for g in ["global", "lists", "run", "plans", "logs", "credentials"] {
        assert!(find(&b, g).is_some(), "{g}");
    }
    assert!(find(&b, "press ? or esc to close").is_some());
}

#[test]
fn feedback_84x10() {
    let mut a = app(Screen::Run);
    key(&mut a, 'f');
    let b = render_buffer(&a, 160, 45);
    at(&b, 38, 15, "╭─ Feedback → Builder · FP-142 ");
    at(&b, 121, 15, "╮");
    at(&b, 38, 24, "╰");
    at(&b, 41, 17, "starts attempt 3/3 with your note + QA report + reviewer verdict");
    assert_eq!(bg_at(&b, 41, 19), FIELD);
    at(&b, 42, 19, "›");
    at(&b, 45, 19, "e.g. make the backoff cap a config value, seed the RNG");
    at(&b, 41, 22, "⏎ send");
    at(&b, 50, 22, "esc cancel");
    for c in "seed it".chars() {
        key(&mut a, c);
    }
    let b = render_buffer(&a, 160, 45);
    at(&b, 44, 19, "seed it");
}

#[test]
fn pair_overlay_renders_qr_half_blocks() {
    let mut a = app(Screen::Dash);
    a.update(Msg::Data(DataEvent::Pair(PairView {
        uri: "kiln://pair?url=https%3A%2F%2Fmac-mini.tailnet.ts.net&code=K7Q29XMA".into(),
        code: "K7Q2-9XMA".into(),
        expires: "12:14".into(),
    })));
    assert!(matches!(a.overlay, Some(Overlay::Pair(Some(_)))));
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "Pair a phone").is_some());
    let (cx, cy) = find(&b, "K7Q2-9XMA").unwrap();
    assert_eq!(fg_at(&b, cx, cy), ACCENT);
    let halves = lines(&b).iter().map(|l| l.matches('▀').count()).sum::<usize>();
    assert!(halves > 300, "{halves}");
    // Small terminal: no QR, code only.
    let b = render_buffer(&a, 80, 24);
    assert!(find(&b, "K7Q2-9XMA").is_some());
    assert!(find(&b, "terminal too small for the QR").is_some());
}

#[test]
fn ansi256_fallback_maps_palette() {
    let a = app(Screen::Dash);
    let mut b = render_buffer(&a, 160, 45);
    theme::downgrade(&mut b);
    assert_eq!(bg_at(&b, 1, 44), Color::Indexed(191));
    assert_eq!(bg_at(&b, 50, 30), Color::Indexed(233));
    assert_eq!(bg_at(&b, 100, 0), Color::Indexed(234));
    assert_eq!(fg_at(&b, 0, 1), Color::Indexed(236));
    assert_eq!(fg_at(&b, 44, 6), Color::Indexed(203));
}

#[test]
fn every_screen_renders_at_many_sizes() {
    for screen in [
        Screen::Login,
        Screen::Setup,
        Screen::Dash,
        Screen::Run,
        Screen::Plans,
        Screen::Chat,
        Screen::Logs,
        Screen::Creds,
    ] {
        for (w, h) in [(160, 45), (80, 24), (120, 36), (200, 60), (100, 30), (60, 20), (20, 8)] {
            let mut a = demo_app(screen, w, h);
            let _ = render_buffer(&a, w, h);
            for ov in [
                Overlay::Palette { q: String::new(), i: 0 },
                Overlay::Help,
                Overlay::Confirm,
                Overlay::Feedback { buf: "x".into() },
                Overlay::Pair(None),
            ] {
                a.overlay = Some(ov);
                let _ = render_buffer(&a, w, h);
            }
        }
    }
}

#[test]
fn empty_live_state_renders() {
    // A signed-in live app before any data arrived.
    let mut a = App::new(false);
    a.update(Msg::Data(DataEvent::SignedIn { email: "a@b.dev".into(), flash: None }));
    for s in [Screen::Dash, Screen::Run, Screen::Plans, Screen::Chat, Screen::Logs, Screen::Creds] {
        a.screen = s;
        let b = render_buffer(&a, 160, 45);
        assert!(row(&b, 0).contains("Kiln"));
    }
    a.screen = Screen::Dash;
    let b = render_buffer(&a, 160, 45);
    assert!(find(&b, "no workers running · dispatch a plan with 3").is_some());
}
