//! `App::update` keymap tests: every key in the TUI keymap table,
//! plus the overlay, INSERT/FILTER and login/setup flows.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kiln_tui::app::{App, FLASH_TICKS, Mode, Msg, Overlay, Screen, default_catalog};
use kiln_tui::model::*;
use kiln_tui::testing::demo_app;
use kiln_types::Status;

fn app(screen: Screen) -> App {
    demo_app(screen, 160, 45)
}

fn k(app: &mut App, c: char) -> Vec<Cmd> {
    let m = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
    app.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), m)))
}

fn code(app: &mut App, kc: KeyCode) -> Vec<Cmd> {
    app.update(Msg::Key(KeyEvent::new(kc, KeyModifiers::NONE)))
}

fn ctrl(app: &mut App, c: char) -> Vec<Cmd> {
    app.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)))
}

fn typ(app: &mut App, s: &str) {
    for c in s.chars() {
        k(app, c);
    }
}

// ---------------------------------------------------------------- global

#[test]
fn digits_switch_screens() {
    let mut a = app(Screen::Dash);
    let expect = [Screen::Dash, Screen::Run, Screen::Plans, Screen::Chat, Screen::Logs, Screen::Creds];
    for (i, s) in expect.iter().enumerate() {
        k(&mut a, char::from(b'1' + i as u8));
        assert_eq!(a.screen, *s);
        assert_eq!(a.mode, Mode::Normal);
    }
}

#[test]
fn entering_run_or_logs_follows_the_task() {
    let mut a = app(Screen::Dash);
    a.followed = None;
    assert_eq!(k(&mut a, '2'), vec![Cmd::Follow("FP-142".into())]);
    // Already followed: no duplicate command.
    assert_eq!(k(&mut a, '5'), vec![]);
}

#[test]
fn palette_opens_with_colon_and_ctrl_k() {
    let mut a = app(Screen::Dash);
    k(&mut a, ':');
    assert!(matches!(a.overlay, Some(Overlay::Palette { .. })));
    code(&mut a, KeyCode::Esc);
    assert!(a.overlay.is_none());
    ctrl(&mut a, 'k');
    assert!(matches!(a.overlay, Some(Overlay::Palette { .. })));
    ctrl(&mut a, 'k');
    assert!(a.overlay.is_none(), "ctrl-k toggles");
}

#[test]
fn palette_filter_select_run() {
    let mut a = app(Screen::Dash);
    k(&mut a, ':');
    typ(&mut a, "logs");
    let Some(Overlay::Palette { q, i }) = &a.overlay else { panic!() };
    assert_eq!((q.as_str(), *i), ("logs", 0));
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.screen, Screen::Logs);
    assert!(a.overlay.is_none());

    // ↑↓ and ctrl-n/p move the selection, clamped.
    k(&mut a, ':');
    code(&mut a, KeyCode::Down);
    ctrl(&mut a, 'n');
    let Some(Overlay::Palette { i, .. }) = &a.overlay else { panic!() };
    assert_eq!(*i, 2);
    ctrl(&mut a, 'p');
    code(&mut a, KeyCode::Up);
    code(&mut a, KeyCode::Up);
    let Some(Overlay::Palette { i, .. }) = &a.overlay else { panic!() };
    assert_eq!(*i, 0);
    // Backspace edits the query and resets the selection.
    typ(&mut a, "xy");
    code(&mut a, KeyCode::Backspace);
    let Some(Overlay::Palette { q, .. }) = &a.overlay else { panic!() };
    assert_eq!(q, "x");
    // Keys don't leak to the screen while the palette is open.
    code(&mut a, KeyCode::Backspace);
    k(&mut a, '3');
    assert_eq!(a.screen, Screen::Logs);
}

#[test]
fn palette_runs_task_commands() {
    let mut a = app(Screen::Dash);
    k(&mut a, ':');
    typ(&mut a, "retry fp");
    assert_eq!(code(&mut a, KeyCode::Enter), vec![Cmd::Retry("FP-142".into())]);
    k(&mut a, ':');
    typ(&mut a, "pair a phone");
    assert_eq!(code(&mut a, KeyCode::Enter), vec![Cmd::PairStart]);
    assert_eq!(a.overlay, Some(Overlay::Pair(None)));
    code(&mut a, KeyCode::Esc);
    assert!(a.overlay.is_none());
}

#[test]
fn help_opens_and_closes() {
    let mut a = app(Screen::Plans);
    k(&mut a, '?');
    assert_eq!(a.overlay, Some(Overlay::Help));
    // Keys are swallowed while help is up.
    k(&mut a, '1');
    assert_eq!(a.screen, Screen::Plans);
    code(&mut a, KeyCode::Esc);
    assert!(a.overlay.is_none());
    k(&mut a, '?');
    k(&mut a, '?');
    assert!(a.overlay.is_none());
    k(&mut a, '?');
    // q closes help instead of signing out.
    assert_eq!(k(&mut a, 'q'), vec![]);
    assert!(a.overlay.is_none());
    assert_eq!(a.screen, Screen::Plans);
}

#[test]
fn q_signs_out() {
    let mut a = app(Screen::Creds);
    a.login.pw = "secret".into();
    assert_eq!(k(&mut a, 'q'), vec![Cmd::Logout]);
    assert_eq!(a.screen, Screen::Login);
    assert_eq!(a.mode, Mode::Insert);
    assert!(a.login.pw.is_empty());
    assert_eq!(a.login.field, 1);
}

#[test]
fn ctrl_c_quits_everywhere() {
    for s in [Screen::Login, Screen::Setup, Screen::Dash, Screen::Chat] {
        let mut a = app(s);
        assert_eq!(ctrl(&mut a, 'c'), vec![Cmd::Quit]);
        assert!(a.quit);
    }
    let mut a = app(Screen::Chat);
    k(&mut a, 'i');
    assert_eq!(ctrl(&mut a, 'c'), vec![Cmd::Quit]);
}

#[test]
fn flash_expires_after_3_2s() {
    let mut a = app(Screen::Dash);
    a.update(Msg::Data(DataEvent::Flash(FlashMsg::ok("✓ hi"))));
    for _ in 0..FLASH_TICKS - 1 {
        a.update(Msg::Tick);
    }
    assert!(a.flash_msg().is_some());
    a.update(Msg::Tick);
    assert!(a.flash_msg().is_none());
    assert_eq!(FLASH_TICKS, 27); // 27 × 120 ms ≈ 3.2 s
}

#[test]
fn spinner_and_blink_follow_the_tick() {
    let mut a = app(Screen::Dash);
    a.tick = 0;
    assert_eq!(a.spinner(), "⠋");
    assert_eq!(a.blink(), "▌");
    a.tick = 5;
    assert_eq!(a.blink(), " ");
    a.tick = 7;
    assert_eq!(a.blink(), " ");
    a.tick = 8;
    assert_eq!(a.blink(), "▌");
    a.tick = 13;
    assert_eq!(a.spinner(), "⠸");
}

// ---------------------------------------------------------------- lists / dashboard

#[test]
fn dashboard_jk_moves_and_enter_opens_run() {
    let mut a = app(Screen::Dash);
    a.dsel = 0;
    k(&mut a, 'k');
    assert_eq!(a.dsel, 0);
    k(&mut a, 'j');
    code(&mut a, KeyCode::Down);
    assert_eq!(a.dsel, 2);
    for _ in 0..10 {
        k(&mut a, 'j');
    }
    assert_eq!(a.dsel, 3, "clamped to the 4 workers");
    code(&mut a, KeyCode::Up);
    assert_eq!(a.dsel, 2);
    a.followed = None;
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(a.screen, Screen::Run);
    assert_eq!(a.run_key.as_deref(), Some("FP-142"));
    assert_eq!(cmds, vec![Cmd::Follow("FP-142".into())]);
    // l also opens.
    let mut a = app(Screen::Dash);
    a.dsel = 0;
    k(&mut a, 'l');
    assert_eq!(a.run_key.as_deref(), Some("FP-145"));
    assert_eq!(a.screen, Screen::Run);
}

// ---------------------------------------------------------------- run

#[test]
fn run_keys() {
    let mut a = app(Screen::Run);
    assert_eq!(a.pane, 3);
    for expect in [0, 1, 2, 3] {
        code(&mut a, KeyCode::Tab);
        assert_eq!(a.pane, expect);
    }
    assert_eq!(k(&mut a, 'r'), vec![Cmd::Retry("FP-142".into())]);
    assert_eq!(k(&mut a, 's'), vec![Cmd::Stop("FP-142".into())]);
    assert_eq!(k(&mut a, 'e'), vec![Cmd::Escalate("FP-142".into())]);
    // o opens the selected artifact from any pane.
    let cmds = k(&mut a, 'o');
    assert!(
        matches!(&cmds[..], [Cmd::OpenArtifact { task, artifact }] if task == "FP-142" && artifact.name == "02-retry-banner.png")
    );
    // Artifacts pane: j/k move, ⏎ opens.
    a.pane = 2;
    k(&mut a, 'j');
    k(&mut a, 'j');
    assert_eq!(a.asel, 3);
    for _ in 0..9 {
        k(&mut a, 'j');
    }
    assert_eq!(a.asel, 4);
    k(&mut a, 'k');
    let cmds = code(&mut a, KeyCode::Enter);
    assert!(matches!(&cmds[..], [Cmd::OpenArtifact { artifact, .. }] if artifact.name == "qa.log"));
    // j/k don't move artifacts in other panes.
    a.pane = 3;
    k(&mut a, 'j');
    assert_eq!(a.asel, 3);
    // L → logs, esc/h → dashboard.
    k(&mut a, 'L');
    assert_eq!(a.screen, Screen::Logs);
    k(&mut a, '2');
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.screen, Screen::Dash);
    k(&mut a, '2');
    k(&mut a, 'h');
    assert_eq!(a.screen, Screen::Dash);
}

#[test]
fn feedback_dialog() {
    let mut a = app(Screen::Run);
    k(&mut a, 'f');
    assert!(matches!(a.overlay, Some(Overlay::Feedback { .. })));
    // Typing goes to the dialog, even global keys.
    typ(&mut a, "seed rng 1");
    code(&mut a, KeyCode::Backspace);
    assert_eq!(a.overlay, Some(Overlay::Feedback { buf: "seed rng ".into() }));
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(cmds, vec![Cmd::Feedback { task: "FP-142".into(), text: "seed rng".into() }]);
    assert!(a.overlay.is_none());
    // Empty feedback just closes; esc cancels.
    k(&mut a, 'f');
    assert_eq!(code(&mut a, KeyCode::Enter), vec![]);
    k(&mut a, 'f');
    typ(&mut a, "x");
    assert_eq!(code(&mut a, KeyCode::Esc), vec![]);
    assert!(a.overlay.is_none());
    // ':' doesn't open the palette over the dialog.
    k(&mut a, 'f');
    k(&mut a, ':');
    assert_eq!(a.overlay, Some(Overlay::Feedback { buf: ":".into() }));
}

// ---------------------------------------------------------------- plans

#[test]
fn plan_tree_navigation_and_expand() {
    let mut a = app(Screen::Plans);
    let plan = a.data.plan.clone().unwrap();
    // T1 (open: 3 ac + 3 workers), T2 (open: 2 + 3), T3 closed.
    assert_eq!(plan.items().len(), 1 + 6 + 1 + 5 + 1);
    k(&mut a, ' ');
    assert!(!a.data.plan.as_ref().unwrap().tasks[0].open);
    assert_eq!(a.data.plan.as_ref().unwrap().items().len(), 1 + 1 + 5 + 1);
    code(&mut a, KeyCode::Enter);
    assert!(a.data.plan.as_ref().unwrap().tasks[0].open);
    // space on a non-task row does nothing.
    k(&mut a, 'j');
    k(&mut a, ' ');
    assert!(a.data.plan.as_ref().unwrap().tasks[0].open);
    for _ in 0..50 {
        k(&mut a, 'j');
    }
    assert_eq!(a.pcur, 13, "clamped to the last row (T3)");
    code(&mut a, KeyCode::Up);
    assert_eq!(a.pcur, 12);
}

#[test]
fn plan_model_and_harness_cycling() {
    let mut a = app(Screen::Plans);
    a.pcur = 4; // T1 builder: claude_sdk / claude-sonnet-5-5
    let w = |a: &App| a.data.plan.as_ref().unwrap().tasks[0].workers[0].clone();
    k(&mut a, 'l');
    assert_eq!(w(&a).model, "claude-opus-5-5");
    k(&mut a, 'l');
    k(&mut a, 'l');
    k(&mut a, 'l');
    assert_eq!(w(&a).model, "qwen3-coder");
    k(&mut a, 'l');
    assert_eq!(w(&a).model, "claude-sonnet-5-5", "wraps around");
    k(&mut a, 'h');
    assert_eq!(w(&a).model, "qwen3-coder", "wraps backwards");
    k(&mut a, 'L');
    assert_eq!(w(&a).harness, "codex");
    k(&mut a, 'L');
    assert_eq!(w(&a).harness, "opencode");
    k(&mut a, 'L');
    assert_eq!(w(&a).harness, "claude_sdk");
    k(&mut a, 'H');
    assert_eq!(w(&a).harness, "opencode");
    assert!(a.plan_dirty);
    // h/l on a non-worker row is a no-op.
    a.pcur = 0;
    let before = a.data.plan.clone();
    k(&mut a, 'l');
    k(&mut a, 'H');
    assert_eq!(a.data.plan, before);
}

#[test]
fn harness_change_resets_model_outside_its_catalog() {
    let mut a = app(Screen::Plans);
    let mut cat = default_catalog();
    cat[1].models = vec!["gpt-5".into(), "gpt-5-codex".into()];
    a.data.catalog = cat;
    a.pcur = 4;
    k(&mut a, 'L'); // claude-sonnet-5-5 isn't a Codex model
    let w = a.data.plan.as_ref().unwrap().tasks[0].workers[0].clone();
    assert_eq!((w.harness.as_str(), w.model.as_str()), ("codex", "gpt-5"));
    k(&mut a, 'l');
    k(&mut a, 'l');
    assert_eq!(a.data.plan.as_ref().unwrap().tasks[0].workers[0].model, "gpt-5", "cycles within Codex models");
}

#[test]
fn plan_edits_survive_a_new_version() {
    let mut a = app(Screen::Plans);
    a.pcur = 4;
    k(&mut a, 'l');
    let mut incoming = a.data.plan.clone().unwrap();
    incoming.version = 4;
    incoming.tasks[0].workers[0].model = "claude-sonnet-5-5".into();
    a.update(Msg::Data(DataEvent::Plan(incoming)));
    let p = a.data.plan.as_ref().unwrap();
    assert_eq!(p.version, 4);
    assert_eq!(p.tasks[0].workers[0].model, "claude-opus-5-5");
}

#[test]
fn approve_confirm_y_n() {
    let mut a = app(Screen::Plans);
    k(&mut a, 'a');
    assert_eq!(a.overlay, Some(Overlay::Confirm));
    k(&mut a, 'n');
    assert!(a.overlay.is_none());
    k(&mut a, 'a');
    code(&mut a, KeyCode::Esc);
    assert!(a.overlay.is_none());
    k(&mut a, 'a');
    // Other keys are swallowed.
    k(&mut a, '1');
    assert_eq!(a.screen, Screen::Plans);
    let cmds = k(&mut a, 'y');
    assert!(matches!(&cmds[..], [Cmd::Approve(p)] if p.tasks.len() == 3));
    assert!(a.overlay.is_none());
    k(&mut a, 'a');
    assert!(matches!(&code(&mut a, KeyCode::Enter)[..], [Cmd::Approve(_)]));
    // The source's Dispatched event returns to the dashboard.
    a.update(Msg::Data(DataEvent::Dispatched));
    assert_eq!(a.screen, Screen::Dash);
    assert_eq!(a.dsel, 0);
}

#[test]
fn plans_n_starts_a_new_idea() {
    let mut a = app(Screen::Plans);
    let cmds = k(&mut a, 'n');
    assert_eq!(a.screen, Screen::Chat);
    assert_eq!(a.mode, Mode::Insert);
    assert!(cmds.contains(&Cmd::NewThread));
    assert!(a.data.chat.msgs.is_empty());
}

// ---------------------------------------------------------------- planner chat

#[test]
fn chat_insert_send_pending() {
    let mut a = app(Screen::Chat);
    k(&mut a, 'x');
    assert_eq!(a.mode, Mode::Normal, "typing in NORMAL does nothing");
    k(&mut a, 'i');
    assert_eq!(a.mode, Mode::Insert);
    // Global keys are text in INSERT.
    typ(&mut a, "q1 ok?");
    assert_eq!(a.screen, Screen::Chat);
    assert_eq!(a.chat.input, "q1 ok?");
    code(&mut a, KeyCode::Backspace);
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(cmds, vec![Cmd::SendMessage { thread: Some("thread-0".into()), text: "q1 ok".into() }]);
    assert!(a.chat.input.is_empty());
    assert_eq!(a.data.chat.msgs.last().unwrap().text, "q1 ok");
    assert!(a.data.chat.pending.is_some());
    // While pending, ⏎ doesn't send.
    typ(&mut a, "more");
    assert_eq!(code(&mut a, KeyCode::Enter), vec![]);
    // The echo with an id doesn't duplicate the optimistic message.
    let n = a.data.chat.msgs.len();
    a.update(Msg::Data(DataEvent::ChatMessage {
        thread: Some("thread-0".into()),
        msg: ChatMsg { id: Some("m9".into()), you: true, text: "q1 ok".into() },
    }));
    assert_eq!(a.data.chat.msgs.len(), n);
    a.update(Msg::Data(DataEvent::PlannerPending { thread: None, label: None }));
    assert!(a.data.chat.pending.is_none());
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::Normal);
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Insert, "⏎ also enters INSERT");
}

#[test]
fn paste_goes_to_the_focused_input() {
    let mut a = app(Screen::Chat);
    k(&mut a, 'i');
    a.update(Msg::Paste("hello\nworld".into()));
    assert_eq!(a.chat.input, "helloworld");
    let mut a = app(Screen::Login);
    a.login.field = 1;
    a.update(Msg::Paste("pw".into()));
    assert_eq!(a.login.pw, "pw");
}

// ---------------------------------------------------------------- logs

#[test]
fn logs_keys() {
    let mut a = app(Screen::Logs);
    assert_eq!(a.logs.lw, 1);
    k(&mut a, 'h');
    assert_eq!(a.logs.lw, 0);
    code(&mut a, KeyCode::Left);
    assert_eq!(a.logs.lw, 0);
    k(&mut a, 'l');
    code(&mut a, KeyCode::Right);
    code(&mut a, KeyCode::Right);
    assert_eq!(a.logs.lw, 2);
    k(&mut a, 'f');
    assert!(!a.logs.follow);
    assert_eq!(a.logs.frozen, a.log_total());
    k(&mut a, 'f');
    assert!(a.logs.follow);
    k(&mut a, 'f');
    k(&mut a, 'G');
    assert!(a.logs.follow, "G jumps to the end and follows");
    k(&mut a, '/');
    assert_eq!(a.mode, Mode::Filter);
    typ(&mut a, "pass1");
    code(&mut a, KeyCode::Backspace);
    assert_eq!(a.logs.filter, "pass");
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.logs.filter, "pass");
    code(&mut a, KeyCode::Esc);
    assert!(a.logs.filter.is_empty(), "esc clears the filter");
    // '/' starts a fresh filter.
    a.logs.filter = "old".into();
    k(&mut a, '/');
    assert!(a.logs.filter.is_empty());
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::Normal);
}

// ---------------------------------------------------------------- credentials

#[test]
fn credentials_keys() {
    let mut a = app(Screen::Creds);
    a.creds.csel = 0;
    k(&mut a, 'j');
    k(&mut a, 'j');
    assert_eq!(a.creds.csel, 2);
    code(&mut a, KeyCode::Tab);
    assert_eq!(a.creds.cfield, 1);
    code(&mut a, KeyCode::Tab);
    assert_eq!(a.creds.cfield, 0, "tab wraps over GitHub's 2 fields");
    code(&mut a, KeyCode::Tab);
    k(&mut a, 'k');
    assert_eq!((a.creds.csel, a.creds.cfield), (1, 0), "moving resets the field");
    for _ in 0..9 {
        k(&mut a, 'j');
    }
    assert_eq!(a.creds.csel, 3);

    // ⏎ / i edit; typing edits the focused field; ⏎ saves.
    k(&mut a, 'i');
    assert_eq!(a.mode, Mode::Insert);
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "XY");
    code(&mut a, KeyCode::Backspace);
    assert_eq!(a.data.creds[3].fields[1].value, "sk-llm-77c0deX");
    let cmds = code(&mut a, KeyCode::Enter);
    assert!(matches!(&cmds[..], [Cmd::SaveCredential(c)] if c.fields[1].value == "sk-llm-77c0deX"));
    assert_eq!(a.mode, Mode::Normal);

    // esc cancels an edit and restores the value.
    code(&mut a, KeyCode::Enter);
    typ(&mut a, "zzz");
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.data.creds[3].fields[1].value, "sk-llm-77c0deX");

    // t tests.
    assert_eq!(k(&mut a, 't'), vec![Cmd::TestCredential("litellm".into())]);
    assert!(a.creds.testing);
    a.update(Msg::Data(DataEvent::CredentialTested {
        key: "litellm".into(),
        status: Status::Passed,
        text: "valid · tested just now · 212ms".into(),
    }));
    assert!(!a.creds.testing);
    assert_eq!(a.data.creds[3].status_text, "valid · tested just now · 212ms");
}

#[test]
fn credentials_new() {
    let mut a = app(Screen::Creds);
    k(&mut a, 'n');
    assert_eq!(a.mode, Mode::Insert);
    assert_eq!(a.creds.csel, 4);
    let c = &a.data.creds[4];
    assert!(c.is_new);
    assert_eq!(c.name, "New credential");
    typ(&mut a, "LiteLLM");
    assert_eq!(a.data.creds[4].name, "LiteLLM");
    assert!(a.data.creds[4].fields.iter().any(|f| f.label == "base url"), "LiteLLM grows a base url field");
    // Testing an unsaved credential is refused.
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.data.creds.len(), 4, "esc discards a new credential");
    k(&mut a, 'n');
    typ(&mut a, "OpenRouter");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "sk-or-1");
    let cmds = code(&mut a, KeyCode::Enter);
    assert!(matches!(&cmds[..], [Cmd::SaveCredential(c)] if c.is_new && c.name == "OpenRouter"));
    assert_eq!(k(&mut a, 't'), vec![]);
    assert!(a.flash_msg().unwrap().text.contains("save the credential first"));
    // The source's saved event replaces the local entry.
    let mut saved = a.data.creds[4].clone();
    saved.is_new = false;
    saved.key = "cred-1".into();
    a.update(Msg::Data(DataEvent::Credential { key: "new-0".into(), cred: saved }));
    assert_eq!(a.data.creds[4].key, "cred-1");
    assert_eq!(a.data.creds.len(), 5);
}

// ---------------------------------------------------------------- login / setup

#[test]
fn login_keys() {
    let mut a = app(Screen::Login);
    a.login.field = 0;
    a.login.email.clear();
    typ(&mut a, "alex@company.dev");
    code(&mut a, KeyCode::Tab);
    assert_eq!(a.login.field, 1);
    typ(&mut a, "pw1");
    code(&mut a, KeyCode::Backspace);
    assert_eq!(a.login.pw, "pw");
    code(&mut a, KeyCode::Up);
    assert_eq!(a.login.field, 0);
    code(&mut a, KeyCode::Down);
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(cmds, vec![Cmd::Login { email: "alex@company.dev".into(), password: "pw".into() }]);
    assert!(a.login.busy);
    // Double enter while busy doesn't resend.
    assert_eq!(code(&mut a, KeyCode::Enter), vec![]);
    a.update(Msg::Data(DataEvent::LoginError { message: "✕ password required".into(), password_field: true }));
    assert!(!a.login.busy);
    assert_eq!(a.login.err, "✕ password required");
    // Typing clears the error; global keys are text here.
    k(&mut a, '1');
    assert!(a.login.err.is_empty());
    assert_eq!(a.screen, Screen::Login);
    a.update(Msg::Data(DataEvent::SignedIn { email: "alex@company.dev".into(), flash: Some(FlashMsg::ok("✓ in")) }));
    assert_eq!(a.screen, Screen::Dash);
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn setup_wizard_flow() {
    let mut a = app(Screen::Setup);
    // Step 0 validation.
    assert_eq!(code(&mut a, KeyCode::Enter), vec![]);
    assert_eq!(a.setup.err, "✕ name required");
    typ(&mut a, "Alex Rao");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "alex@company.dev");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "short");
    code(&mut a, KeyCode::Enter);
    assert!(a.setup.err.contains("at least 10"));
    assert_eq!(a.setup.field, 2);
    typ(&mut a, "-but-longer");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "nope");
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.setup.err, "✕ passwords don't match");
    for _ in 0..4 {
        code(&mut a, KeyCode::Backspace);
    }
    typ(&mut a, "short-but-longer");
    code(&mut a, KeyCode::Enter);
    assert_eq!(a.setup.step, 1);
    assert_eq!(a.setup.fields[1][0], "company.dev", "domain prefilled from the admin email");
    // Esc goes back a step, enter returns.
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.setup.step, 0);
    code(&mut a, KeyCode::Enter);
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Setup {
            name: "Alex Rao".into(),
            email: "alex@company.dev".into(),
            password: "short-but-longer".into(),
            domain: "company.dev".into()
        }]
    );
    assert!(a.setup.busy);
    a.update(Msg::Data(DataEvent::SignedIn { email: "alex@company.dev".into(), flash: None }));
    assert_eq!(a.screen, Screen::Setup, "stays in the wizard for step 3");
    a.update(Msg::Data(DataEvent::SetupStep { step: 0 }));
    assert_eq!(a.setup.step, 2);
    typ(&mut a, "f");
    code(&mut a, KeyCode::Enter);
    assert!(a.setup.err.contains("2–6"));
    typ(&mut a, "p");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "Acme CLI");
    code(&mut a, KeyCode::Tab);
    typ(&mut a, "git@github.com:acme/fp.git");
    let cmds = code(&mut a, KeyCode::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::RegisterProject {
            key: "FP".into(),
            name: "Acme CLI".into(),
            repo_url: "git@github.com:acme/fp.git".into(),
            branch: "main".into(),
            qa_path: "qa.yaml".into()
        }]
    );
    a.update(Msg::Data(DataEvent::SetupStep { step: 1 }));
    assert_eq!(a.screen, Screen::Dash);
}

#[test]
fn setup_project_step_can_be_skipped() {
    let mut a = app(Screen::Setup);
    a.setup.step = 2;
    code(&mut a, KeyCode::Esc);
    assert_eq!(a.screen, Screen::Dash);
    assert!(a.flash_msg().unwrap().text.contains("skipped"));
}

#[test]
fn setup_required_event_opens_wizard() {
    let mut a = App::new(false);
    a.update(Msg::Data(DataEvent::SetupRequired { allowed_domain: None }));
    assert_eq!(a.screen, Screen::Setup);
    assert_eq!(a.mode, Mode::Insert);
}

#[test]
fn signed_out_event_returns_to_login() {
    let mut a = app(Screen::Run);
    a.overlay = Some(Overlay::Help);
    a.update(Msg::Data(DataEvent::SignedOut));
    assert_eq!(a.screen, Screen::Login);
    assert!(a.overlay.is_none());
}

// ---------------------------------------------------------------- compact layouts

#[test]
fn compact_tab_cycles_collapsed_panes() {
    let mut a = demo_app(Screen::Dash, 80, 24);
    assert!(a.compact());
    for expect in [1, 2, 0] {
        code(&mut a, KeyCode::Tab);
        assert_eq!(a.side, expect);
    }
    let mut a = demo_app(Screen::Plans, 80, 24);
    code(&mut a, KeyCode::Tab);
    assert_eq!(a.side, 1);
    // At full size tab does nothing on the dashboard.
    let mut a = app(Screen::Dash);
    code(&mut a, KeyCode::Tab);
    assert_eq!(a.side, 0);
    // Resize switches layouts.
    a.update(Msg::Resize(80, 24));
    assert!(a.compact());
    a.update(Msg::Resize(160, 45));
    assert!(!a.compact());
}
