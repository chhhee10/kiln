//! Elm-style application state: [`App`] + [`App::update`]. Rendering lives in
//! [`crate::view`] and only reads `&App`.
//!
//! The keymap is a port of the prototype's `onKey` (design/designs/TUI.dc.html)
//! and the keymap table in design/docs/tui.md.

use crate::model::*;
use crate::palette::{self, PalAction};
use crate::theme::Tone;
use chrono::{DateTime, Local};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use kiln_types::Status;

/// Ticks a flash message stays on the status line (3.2 s at 120 ms).
pub const FLASH_TICKS: u64 = 27;
/// Planner pending label shown until the source reports its own.
pub const PENDING_LABEL: &str = "reading company brain · drafting changes…";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Login,
    Setup,
    Dash,
    Run,
    Plans,
    Chat,
    Logs,
    Creds,
}

/// Header tabs, in key order 1–6.
pub const TABS: [(Screen, &str); 6] = [
    (Screen::Dash, "Dashboard"),
    (Screen::Run, "Run"),
    (Screen::Plans, "Plans"),
    (Screen::Chat, "Planner"),
    (Screen::Logs, "Logs"),
    (Screen::Creds, "Credentials"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Filter,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    Palette { q: String, i: usize },
    Help,
    Feedback { buf: String },
    Confirm,
    Pair(Option<PairView>),
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // short-lived, consumed by update()
pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Tick,
    Resize(u16, u16),
    Data(DataEvent),
}

#[derive(Clone, Debug, Default)]
pub struct LoginUi {
    pub email: String,
    pub pw: String,
    pub field: usize,
    pub err: String,
    pub busy: bool,
}

/// First-run wizard (PRD §4.1).
#[derive(Clone, Debug)]
pub struct SetupUi {
    pub step: usize,
    pub fields: [Vec<String>; 3],
    pub field: usize,
    pub err: String,
    pub busy: bool,
}

pub const SETUP_LABELS: [&[&str]; 3] = [
    &["your name", "email", "password", "confirm password"],
    &["allowed email domain"],
    &["project key", "name", "repo url", "default branch", "qa.yaml path"],
];

impl Default for SetupUi {
    fn default() -> Self {
        Self {
            step: 0,
            fields: [
                vec![String::new(); 4],
                vec![String::new()],
                vec![String::new(), String::new(), String::new(), "main".into(), "qa.yaml".into()],
            ],
            field: 0,
            err: String::new(),
            busy: false,
        }
    }
}

impl SetupUi {
    pub fn is_secret(step: usize, field: usize) -> bool {
        step == 0 && field >= 2
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChatUi {
    pub input: String,
}

#[derive(Clone, Debug)]
pub struct LogsUi {
    /// Selected worker tab (0 builder, 1 qa, 2 reviewer).
    pub lw: usize,
    pub follow: bool,
    /// Line index at which the view was paused.
    pub frozen: usize,
    pub filter: String,
}

impl Default for LogsUi {
    fn default() -> Self {
        Self { lw: 1, follow: true, frozen: 0, filter: String::new() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CredsUi {
    pub csel: usize,
    pub cfield: usize,
    pub testing: bool,
    /// Snapshot taken when INSERT started, restored on esc.
    pub before_edit: Option<CredView>,
}

pub struct App {
    pub demo: bool,
    pub screen: Screen,
    pub mode: Mode,
    pub tick: u64,
    pub now: DateTime<Local>,
    pub width: u16,
    pub height: u16,
    pub data: Data,
    pub login: LoginUi,
    pub setup: SetupUi,
    /// Running-workers selection on the dashboard.
    pub dsel: usize,
    /// Task shown on Run / Logs.
    pub run_key: Option<String>,
    /// Run pane: 0 stages, 1 tests, 2 artifacts, 3 log.
    pub pane: usize,
    pub asel: usize,
    /// Compact layouts: which collapsed pane is visible.
    pub side: usize,
    pub pcur: usize,
    pub plan_dirty: bool,
    pub chat: ChatUi,
    pub logs: LogsUi,
    pub creds: CredsUi,
    pub overlay: Option<Overlay>,
    pub flash: Option<(FlashMsg, u64)>,
    pub followed: Option<String>,
    pub quit: bool,
    pub session_path: String,
}

impl App {
    pub fn new(demo: bool) -> Self {
        Self {
            demo,
            screen: Screen::Login,
            mode: Mode::Insert,
            tick: 0,
            now: Local::now(),
            width: 160,
            height: 45,
            data: Data::default(),
            login: LoginUi { field: 1, ..Default::default() },
            setup: SetupUi::default(),
            dsel: 0,
            run_key: None,
            pane: 3,
            asel: 1,
            side: 0,
            pcur: 0,
            plan_dirty: false,
            chat: ChatUi::default(),
            logs: LogsUi::default(),
            creds: CredsUi::default(),
            overlay: None,
            flash: None,
            followed: None,
            quit: false,
            session_path: "~/.config/kiln/session.toml".into(),
        }
    }

    /// Below 120×36 the right-hand panes collapse into tabs (tui.md "Responsive").
    pub fn compact(&self) -> bool {
        self.width < 120 || self.height < 36
    }

    /// The dashboard needs more width: the 7-column board next to the
    /// 56-wide Machine pane stops fitting below 140 columns.
    pub fn dash_compact(&self) -> bool {
        self.compact() || self.width < 140
    }

    pub fn spinner(&self) -> &'static str {
        crate::theme::SPINNER[(self.tick % 10) as usize]
    }

    /// Blinking cursor: 5 ticks on, 3 off.
    pub fn blink(&self) -> &'static str {
        if self.tick % 8 < 5 { "▌" } else { " " }
    }

    /// The task shown on Run / Logs (prototype `cur()`).
    pub fn cur(&self) -> Option<&TaskView> {
        self.run_key
            .as_deref()
            .and_then(|k| self.data.task(k))
            .or_else(|| self.data.workers().into_iter().next())
            .or_else(|| self.data.tasks.first())
    }

    pub fn cur_key(&self) -> Option<String> {
        self.cur().map(|t| t.key.clone())
    }

    pub fn flash(&mut self, m: FlashMsg) {
        self.flash = Some((m, self.tick + FLASH_TICKS));
    }

    pub fn flash_msg(&self) -> Option<&FlashMsg> {
        self.flash.as_ref().map(|(m, _)| m)
    }

    fn go(&mut self, screen: Screen) -> Vec<Cmd> {
        self.screen = screen;
        self.mode = Mode::Normal;
        self.overlay = None;
        self.side = 0;
        let mut cmds = Vec::new();
        if matches!(screen, Screen::Run | Screen::Logs) {
            cmds.extend(self.follow_cur());
        }
        if screen == Screen::Plans
            && self.data.plan.is_none()
            && let Some(p) = self.data.plans.first()
        {
            cmds.push(Cmd::OpenPlan(p.key.clone()));
        }
        cmds
    }

    fn follow_cur(&mut self) -> Vec<Cmd> {
        match self.cur_key() {
            Some(k) if self.followed.as_deref() != Some(k.as_str()) => {
                self.followed = Some(k.clone());
                vec![Cmd::Follow(k)]
            }
            _ => vec![],
        }
    }

    fn open_run(&mut self, key: String) -> Vec<Cmd> {
        self.run_key = Some(key);
        self.pane = 3;
        self.go(Screen::Run)
    }

    /// Total lines received for the current task.
    pub fn log_total(&self) -> usize {
        self.cur().and_then(|t| self.data.logs.get(&t.key)).and_then(|v| v.last()).map(|l| l.idx + 1).unwrap_or(0)
    }

    // ------------------------------------------------------------------ update

    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Tick => {
                self.tick += 1;
                self.now = Local::now();
                if let Some((_, until)) = &self.flash
                    && self.tick >= *until
                {
                    self.flash = None;
                }
                vec![]
            }
            Msg::Resize(w, h) => {
                self.width = w;
                self.height = h;
                vec![]
            }
            Msg::Data(ev) => self.on_data(ev),
            Msg::Paste(s) => {
                self.on_paste(&s);
                vec![]
            }
            Msg::Key(k) => {
                if k.kind == KeyEventKind::Release {
                    return vec![];
                }
                self.on_key(k)
            }
        }
    }

    fn on_paste(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        if let Some(ov) = &mut self.overlay {
            match ov {
                Overlay::Palette { q, i } => {
                    q.push_str(&s);
                    *i = 0;
                }
                Overlay::Feedback { buf } => buf.push_str(&s),
                _ => {}
            }
            return;
        }
        match self.screen {
            Screen::Login => {
                if self.login.field == 0 {
                    self.login.email.push_str(&s)
                } else {
                    self.login.pw.push_str(&s)
                }
            }
            Screen::Setup => {
                let (st, f) = (self.setup.step, self.setup.field);
                self.setup.fields[st][f].push_str(&s);
            }
            _ if self.mode == Mode::Normal => {}
            Screen::Chat => self.chat.input.push_str(&s),
            Screen::Logs => self.logs.filter.push_str(&s),
            Screen::Creds => self.edit_cred(|v| v.push_str(&s)),
            _ => {}
        }
    }

    fn on_data(&mut self, ev: DataEvent) -> Vec<Cmd> {
        let tasks_changed = matches!(ev, DataEvent::Tasks(_) | DataEvent::TaskUpsert(_) | DataEvent::TaskRemove(_));
        let d = &mut self.data;
        match ev {
            DataEvent::Server(s) => d.server = s,
            DataEvent::SetupRequired { allowed_domain } => {
                d.server.allowed_domain = allowed_domain;
                self.screen = Screen::Setup;
                self.mode = Mode::Insert;
            }
            DataEvent::SignedIn { email, flash } => {
                d.user_email = email;
                self.login.busy = false;
                self.login.err.clear();
                if self.screen == Screen::Login {
                    self.screen = Screen::Dash;
                    self.mode = Mode::Normal;
                }
                if let Some(f) = flash {
                    self.flash(f);
                }
            }
            DataEvent::LoginError { message, password_field } => {
                self.login.busy = false;
                self.login.err = message;
                if password_field {
                    self.login.field = 1;
                }
            }
            DataEvent::SetupStep { step } => {
                self.setup.busy = false;
                self.setup.err.clear();
                if step == 0 {
                    self.setup.step = 2;
                    self.setup.field = 0;
                } else {
                    self.screen = Screen::Dash;
                    self.mode = Mode::Normal;
                }
            }
            DataEvent::SetupError(e) => {
                self.setup.busy = false;
                self.setup.err = e;
            }
            DataEvent::SignedOut => {
                self.screen = Screen::Login;
                self.mode = Mode::Insert;
                self.login.pw.clear();
                self.login.field = 1;
                self.overlay = None;
            }
            DataEvent::Tasks(t) => d.tasks = t,
            DataEvent::TaskUpsert(t) => d.upsert_task(t),
            DataEvent::TaskRemove(k) => d.tasks.retain(|t| t.key != k),
            DataEvent::Machine(m) => d.machine = m,
            DataEvent::Inbox(i) => d.inbox = i,
            DataEvent::Activity(a) => {
                d.activity.insert(0, a);
                d.activity.truncate(100);
            }
            DataEvent::ActivityAll(a) => d.activity = a,
            DataEvent::Plans(p) => d.plans = p,
            DataEvent::Plan(p) => {
                let keep_edits = self.plan_dirty && d.plan.as_ref().is_some_and(|x| x.key == p.key);
                match (&mut d.plan, keep_edits) {
                    (Some(cur), true) => {
                        cur.version = p.version;
                        cur.status = p.status;
                    }
                    _ => {
                        if d.plan.as_ref().is_none_or(|x| x.key != p.key) {
                            self.pcur = 0;
                            self.plan_dirty = false;
                        }
                        d.plan = Some(p);
                    }
                }
                let n = d.plan.as_ref().map(|p| p.items().len()).unwrap_or(0);
                self.pcur = self.pcur.min(n.saturating_sub(1));
            }
            DataEvent::Catalog(c) => d.catalog = c,
            DataEvent::Threads(t) => d.threads = t,
            DataEvent::Conversation(c) => d.chat = c,
            DataEvent::ChatMessage { thread, msg } => {
                if thread.is_some() && d.chat.thread.is_none() {
                    d.chat.thread = thread.clone();
                }
                if thread.is_none() || thread == d.chat.thread {
                    let dup = msg.id.is_some() && d.chat.msgs.iter().any(|m| m.id == msg.id);
                    if !dup {
                        // Our own optimistic message gets its id on echo.
                        if let Some(local) = d
                            .chat
                            .msgs
                            .iter_mut()
                            .rev()
                            .find(|m| m.id.is_none() && m.you == msg.you && m.text == msg.text)
                        {
                            local.id = msg.id;
                        } else {
                            d.chat.msgs.push(msg);
                        }
                    }
                }
            }
            DataEvent::PlannerPending { thread, label } => {
                if thread.is_none() || thread == d.chat.thread {
                    d.chat.pending = label;
                }
            }
            DataEvent::Run(r) => {
                d.run = Some(r);
                let n = d.run.as_ref().map(|r| r.artifacts.len()).unwrap_or(0);
                self.asel = self.asel.min(n.saturating_sub(1));
            }
            DataEvent::LogBackfill { task, lines } => {
                d.logs.remove(&task);
                for l in lines {
                    d.push_log(&task, l);
                }
            }
            DataEvent::Log { task, line } => d.push_log(&task, line),
            DataEvent::Credentials(c) => {
                // Keep unsaved local credentials.
                let local: Vec<CredView> = d.creds.iter().filter(|c| c.is_new).cloned().collect();
                d.creds = c;
                d.creds.extend(local);
                self.creds.csel = self.creds.csel.min(d.creds.len().saturating_sub(1));
            }
            DataEvent::Credential { key, cred } => match d.creds.iter_mut().find(|c| c.key == key) {
                Some(c) => *c = cred,
                None => d.creds.push(cred),
            },
            DataEvent::CredentialTested { key, status, text } => {
                self.creds.testing = false;
                if let Some(c) = d.creds.iter_mut().find(|c| c.key == key) {
                    c.status = status;
                    c.status_text = text;
                }
            }
            DataEvent::Checklist(c) => d.checklist = c,
            DataEvent::Pair(p) => self.overlay = Some(Overlay::Pair(Some(p))),
            DataEvent::Dispatched => {
                self.overlay = None;
                self.plan_dirty = false;
                self.screen = Screen::Dash;
                self.mode = Mode::Normal;
                self.dsel = 0;
            }
            DataEvent::Flash(f) => {
                if f.tone == Tone::FAILED {
                    self.creds.testing = false;
                    self.login.busy = false;
                    self.setup.busy = false;
                    if self.overlay == Some(Overlay::Pair(None)) {
                        self.overlay = None;
                    }
                }
                self.flash(f)
            }
            DataEvent::Synced => d.last_sync = Some(Local::now()),
        }
        if tasks_changed {
            let n = self.data.workers().len();
            self.dsel = self.dsel.min(n.saturating_sub(1));
        }
        vec![]
    }

    // ------------------------------------------------------------------ keys

    fn on_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if ctrl && matches!(k.code, KeyCode::Char('c')) {
            self.quit = true;
            return vec![Cmd::Quit];
        }
        let ch = match k.code {
            KeyCode::Char(c) if !ctrl && !alt => Some(c),
            _ => None,
        };

        match self.screen {
            Screen::Login => return self.key_login(k.code, ch),
            Screen::Setup => return self.key_setup(k.code, ch),
            _ => {}
        }

        let palette_open = matches!(self.overlay, Some(Overlay::Palette { .. }));
        let blocking = matches!(self.overlay, Some(Overlay::Feedback { .. } | Overlay::Confirm));
        if (ctrl && k.code == KeyCode::Char('k'))
            || (ch == Some(':') && self.mode == Mode::Normal && !palette_open && !blocking)
        {
            self.overlay = if palette_open { None } else { Some(Overlay::Palette { q: String::new(), i: 0 }) };
            return vec![];
        }

        if let Some(ov) = self.overlay.clone() {
            return self.key_overlay(ov, k.code, ch, ctrl);
        }

        match self.mode {
            Mode::Insert | Mode::Filter => return self.key_insert(k.code, ch),
            Mode::Normal => {}
        }

        let code = k.code;
        let navigational = matches!(
            code,
            KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Down
                | KeyCode::Up
                | KeyCode::Left
                | KeyCode::Right
        );
        if ch.is_none() && !navigational {
            return vec![];
        }
        if let Some(c @ '1'..='6') = ch {
            let i = c as usize - '1' as usize;
            return self.go(TABS[i].0);
        }
        if ch == Some('?') {
            self.overlay = Some(Overlay::Help);
            return vec![];
        }
        if ch == Some('q') {
            return self.sign_out();
        }
        let down = ch == Some('j') || code == KeyCode::Down;
        let up = ch == Some('k') || code == KeyCode::Up;
        let tab = code == KeyCode::Tab;
        let enter = code == KeyCode::Enter;
        let esc = code == KeyCode::Esc;

        match self.screen {
            Screen::Dash => {
                let n = self.data.workers().len();
                if down {
                    self.dsel = (self.dsel + 1).min(n.saturating_sub(1));
                }
                if up {
                    self.dsel = self.dsel.saturating_sub(1);
                }
                if tab && self.dash_compact() {
                    self.side = (self.side + 1) % 3;
                }
                if (enter || ch == Some('l'))
                    && let Some(w) = self.data.workers().get(self.dsel)
                {
                    let key = w.key.clone();
                    return self.open_run(key);
                }
            }
            Screen::Run => {
                if tab {
                    self.pane = (self.pane + 1) % 4;
                }
                let arts = self.data.run.as_ref().map(|r| r.artifacts.len()).unwrap_or(0);
                if self.pane == 2 {
                    if down {
                        self.asel = (self.asel + 1).min(arts.saturating_sub(1));
                    }
                    if up {
                        self.asel = self.asel.saturating_sub(1);
                    }
                }
                if ch == Some('o') || (enter && self.pane == 2) {
                    return self.open_artifact();
                }
                match ch {
                    Some('r') => return self.task_cmd(Cmd::Retry),
                    Some('s') => return self.task_cmd(Cmd::Stop),
                    Some('e') => return self.task_cmd(Cmd::Escalate),
                    Some('f') if self.cur().is_some() => {
                        self.overlay = Some(Overlay::Feedback { buf: String::new() });
                    }
                    Some('L') => return self.go(Screen::Logs),
                    Some('h') => return self.go(Screen::Dash),
                    _ => {}
                }
                if esc {
                    return self.go(Screen::Dash);
                }
            }
            Screen::Plans => {
                let n = self.data.plan.as_ref().map(|p| p.items().len()).unwrap_or(0);
                if down {
                    self.pcur = (self.pcur + 1).min(n.saturating_sub(1));
                }
                if up {
                    self.pcur = self.pcur.saturating_sub(1);
                }
                if tab && self.compact() {
                    self.side = (self.side + 1) % 2;
                }
                if ch == Some(' ') || enter {
                    self.toggle_plan_task();
                }
                match ch {
                    Some('l') => self.cycle_worker(1, false),
                    Some('h') => self.cycle_worker(-1, false),
                    Some('L') => self.cycle_worker(1, true),
                    Some('H') => self.cycle_worker(-1, true),
                    Some('a') if self.data.plan.is_some() => self.overlay = Some(Overlay::Confirm),
                    Some('n') => return self.new_thread(),
                    _ => {}
                }
            }
            Screen::Chat => {
                if ch == Some('i') || enter {
                    self.mode = Mode::Insert;
                }
                if tab && self.compact() {
                    self.side = (self.side + 1) % 2;
                }
            }
            Screen::Logs => {
                if ch == Some('h') || code == KeyCode::Left {
                    self.logs.lw = self.logs.lw.saturating_sub(1);
                }
                if ch == Some('l') || code == KeyCode::Right {
                    self.logs.lw = (self.logs.lw + 1).min(2);
                }
                if ch == Some('f') {
                    self.logs.follow = !self.logs.follow;
                    self.logs.frozen = self.log_total();
                }
                if ch == Some('G') {
                    self.logs.follow = true;
                }
                if ch == Some('/') {
                    self.mode = Mode::Filter;
                    self.logs.filter.clear();
                }
                if esc {
                    self.logs.filter.clear();
                }
            }
            Screen::Creds => {
                let n = self.data.creds.len();
                if down {
                    self.creds.csel = (self.creds.csel + 1).min(n.saturating_sub(1));
                    self.creds.cfield = 0;
                }
                if up {
                    self.creds.csel = self.creds.csel.saturating_sub(1);
                    self.creds.cfield = 0;
                }
                if tab {
                    let nf = self.data.creds.get(self.creds.csel).map(|c| c.fields.len()).unwrap_or(1).max(1);
                    self.creds.cfield = (self.creds.cfield + 1) % nf;
                }
                if (enter || ch == Some('i')) && n > 0 {
                    self.start_cred_edit();
                }
                if ch == Some('t') {
                    return self.test_cred();
                }
                if ch == Some('n') {
                    self.add_cred();
                }
            }
            Screen::Login | Screen::Setup => {}
        }
        vec![]
    }

    fn key_overlay(&mut self, ov: Overlay, code: KeyCode, ch: Option<char>, ctrl: bool) -> Vec<Cmd> {
        match ov {
            Overlay::Palette { mut q, mut i } => {
                let cmds = palette::commands(self, &q);
                match code {
                    KeyCode::Esc => {
                        self.overlay = None;
                        return vec![];
                    }
                    KeyCode::Down => i = (i + 1).min(cmds.len().saturating_sub(1)),
                    KeyCode::Char('n') if ctrl => i = (i + 1).min(cmds.len().saturating_sub(1)),
                    KeyCode::Up => i = i.saturating_sub(1),
                    KeyCode::Char('p') if ctrl => i = i.saturating_sub(1),
                    KeyCode::Enter => {
                        self.overlay = None;
                        return match cmds.get(i) {
                            Some(c) => self.run_action(c.action.clone()),
                            None => vec![],
                        };
                    }
                    KeyCode::Backspace => {
                        q.pop();
                        i = 0;
                    }
                    _ => {
                        if let Some(c) = ch {
                            q.push(c);
                            i = 0;
                        }
                    }
                }
                self.overlay = Some(Overlay::Palette { q, i });
            }
            Overlay::Feedback { mut buf } => match code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Enter => {
                    self.overlay = None;
                    let text = buf.trim().to_string();
                    if !text.is_empty()
                        && let Some(key) = self.cur_key()
                    {
                        return vec![Cmd::Feedback { task: key, text }];
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.overlay = Some(Overlay::Feedback { buf });
                }
                _ => {
                    if let Some(c) = ch {
                        buf.push(c);
                    }
                    self.overlay = Some(Overlay::Feedback { buf });
                }
            },
            Overlay::Confirm => match (code, ch) {
                (KeyCode::Enter, _) | (_, Some('y')) => {
                    self.overlay = None;
                    if let Some(p) = self.data.plan.clone() {
                        return vec![Cmd::Approve(p)];
                    }
                }
                (KeyCode::Esc, _) | (_, Some('n')) => self.overlay = None,
                _ => {}
            },
            Overlay::Help => {
                if code == KeyCode::Esc || ch == Some('?') || ch == Some('q') {
                    self.overlay = None;
                }
            }
            Overlay::Pair(_) => {
                if code == KeyCode::Esc || code == KeyCode::Enter || ch == Some('q') {
                    self.overlay = None;
                }
            }
        }
        vec![]
    }

    fn key_insert(&mut self, code: KeyCode, ch: Option<char>) -> Vec<Cmd> {
        if code == KeyCode::Esc {
            if self.screen == Screen::Creds {
                self.cancel_cred_edit();
            }
            self.mode = Mode::Normal;
            return vec![];
        }
        match self.screen {
            Screen::Chat => match code {
                KeyCode::Enter => return self.send_chat(),
                KeyCode::Backspace => {
                    self.chat.input.pop();
                }
                _ => {
                    if let Some(c) = ch {
                        self.chat.input.push(c);
                    }
                }
            },
            Screen::Logs => match code {
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.logs.filter.pop();
                }
                _ => {
                    if let Some(c) = ch {
                        self.logs.filter.push(c);
                    }
                }
            },
            Screen::Creds => match code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.creds.before_edit = None;
                    self.sync_new_cred_name();
                    if let Some(c) = self.data.creds.get(self.creds.csel) {
                        return vec![Cmd::SaveCredential(c.clone())];
                    }
                }
                KeyCode::Tab => {
                    let nf = self.data.creds.get(self.creds.csel).map(|c| c.fields.len()).unwrap_or(1).max(1);
                    self.creds.cfield = (self.creds.cfield + 1) % nf;
                }
                KeyCode::Backspace => self.edit_cred(|v| {
                    v.pop();
                }),
                _ => {
                    if let Some(c) = ch {
                        self.edit_cred(|v| v.push(c));
                    }
                }
            },
            _ => self.mode = Mode::Normal,
        }
        vec![]
    }

    fn key_login(&mut self, code: KeyCode, ch: Option<char>) -> Vec<Cmd> {
        let l = &mut self.login;
        match code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => l.field = 1 - l.field.min(1),
            KeyCode::Enter => {
                if l.busy {
                    return vec![];
                }
                l.busy = true;
                return vec![Cmd::Login { email: l.email.trim().to_string(), password: l.pw.clone() }];
            }
            KeyCode::Backspace => {
                if l.field == 0 {
                    l.email.pop();
                } else {
                    l.pw.pop();
                }
            }
            _ => {
                if let Some(c) = ch {
                    if l.field == 0 {
                        l.email.push(c);
                    } else {
                        l.pw.push(c);
                    }
                    l.err.clear();
                }
            }
        }
        vec![]
    }

    fn key_setup(&mut self, code: KeyCode, ch: Option<char>) -> Vec<Cmd> {
        let s = &mut self.setup;
        let nf = s.fields[s.step].len();
        match code {
            KeyCode::Tab | KeyCode::Down => s.field = (s.field + 1) % nf,
            KeyCode::BackTab | KeyCode::Up => s.field = (s.field + nf - 1) % nf,
            KeyCode::Esc => match s.step {
                1 => {
                    s.step = 0;
                    s.field = 0;
                    s.err.clear();
                }
                2 => {
                    self.screen = Screen::Dash;
                    self.mode = Mode::Normal;
                    self.flash(FlashMsg::new(
                        "○ project registration skipped · add projects later as admin",
                        Tone::QUEUED,
                    ));
                }
                _ => {}
            },
            KeyCode::Enter => return self.submit_setup(),
            KeyCode::Backspace => {
                s.fields[s.step][s.field].pop();
            }
            _ => {
                if let Some(c) = ch {
                    s.fields[s.step][s.field].push(c);
                    s.err.clear();
                }
            }
        }
        vec![]
    }

    fn submit_setup(&mut self) -> Vec<Cmd> {
        let s = &mut self.setup;
        if s.busy {
            return vec![];
        }
        let f = |i: usize| s.fields[s.step][i].trim().to_string();
        match s.step {
            0 => {
                let (name, email, pw, pw2) = (f(0), f(1), s.fields[0][2].clone(), s.fields[0][3].clone());
                let err = if name.is_empty() {
                    Some((0, "✕ name required"))
                } else if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
                    Some((1, "✕ enter a valid email"))
                } else if pw.chars().count() < 10 {
                    Some((2, "✕ password must be at least 10 characters"))
                } else if pw != pw2 {
                    Some((3, "✕ passwords don't match"))
                } else {
                    None
                };
                if let Some((field, e)) = err {
                    s.err = e.into();
                    s.field = field;
                    return vec![];
                }
                if s.fields[1][0].is_empty() {
                    s.fields[1][0] = email.rsplit('@').next().unwrap_or_default().to_string();
                }
                s.step = 1;
                s.field = 0;
                s.err.clear();
                vec![]
            }
            1 => {
                let domain = f(0).trim_start_matches('@').to_string();
                if domain.is_empty() || !domain.contains('.') {
                    s.err = "✕ enter a domain like company.dev".into();
                    return vec![];
                }
                s.busy = true;
                vec![Cmd::Setup {
                    name: f_owned(&s.fields[0][0]),
                    email: f_owned(&s.fields[0][1]),
                    password: s.fields[0][2].clone(),
                    domain,
                }]
            }
            _ => {
                let key = f(0).to_uppercase();
                let valid_key = (2..=6).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric());
                let err = if !valid_key {
                    Some((0, "✕ key is 2–6 letters or digits, e.g. FP"))
                } else if f(1).is_empty() {
                    Some((1, "✕ project name required"))
                } else if f(2).is_empty() {
                    Some((2, "✕ repo url required"))
                } else {
                    None
                };
                if let Some((field, e)) = err {
                    s.err = e.into();
                    s.field = field;
                    return vec![];
                }
                s.busy = true;
                let branch = if f(3).is_empty() { "main".into() } else { f(3) };
                let qa = if f(4).is_empty() { "qa.yaml".into() } else { f(4) };
                vec![Cmd::RegisterProject { key, name: f(1), repo_url: f(2), branch, qa_path: qa }]
            }
        }
    }

    // ------------------------------------------------------------------ actions

    fn sign_out(&mut self) -> Vec<Cmd> {
        self.screen = Screen::Login;
        self.mode = Mode::Insert;
        self.overlay = None;
        self.login.pw.clear();
        self.login.field = 1;
        self.login.busy = false;
        self.followed = None;
        vec![Cmd::Logout]
    }

    fn task_cmd(&mut self, f: fn(String) -> Cmd) -> Vec<Cmd> {
        match self.cur_key() {
            Some(k) => vec![f(k)],
            None => vec![],
        }
    }

    fn open_artifact(&mut self) -> Vec<Cmd> {
        let (Some(task), Some(run)) = (self.cur_key(), self.data.run.as_ref()) else {
            return vec![];
        };
        match run.artifacts.get(self.asel) {
            Some(a) => vec![Cmd::OpenArtifact { task, artifact: a.clone() }],
            None => vec![],
        }
    }

    fn new_thread(&mut self) -> Vec<Cmd> {
        let mut cmds = self.go(Screen::Chat);
        self.data.chat = Conversation { title: "New idea".into(), ..Default::default() };
        self.mode = Mode::Insert;
        cmds.push(Cmd::NewThread);
        cmds
    }

    fn send_chat(&mut self) -> Vec<Cmd> {
        let text = self.chat.input.trim().to_string();
        if text.is_empty() || self.data.chat.pending.is_some() {
            return vec![];
        }
        self.chat.input.clear();
        self.data.chat.msgs.push(ChatMsg { id: None, you: true, text: text.clone() });
        self.data.chat.pending = Some(PENDING_LABEL.into());
        vec![Cmd::SendMessage { thread: self.data.chat.thread.clone(), text }]
    }

    fn toggle_plan_task(&mut self) {
        let Some(plan) = self.data.plan.as_mut() else {
            return;
        };
        if let Some(PlanItem::Task(ti)) = plan.items().get(self.pcur).copied() {
            plan.tasks[ti].open = !plan.tasks[ti].open;
        }
    }

    /// h/l cycles the model, H/L the harness, on the selected worker row.
    pub fn cycle_worker(&mut self, dir: i32, harness: bool) {
        let catalog = if self.data.catalog.is_empty() { default_catalog() } else { self.data.catalog.clone() };
        let Some(plan) = self.data.plan.as_mut() else {
            return;
        };
        let Some(PlanItem::Worker(ti, wi)) = plan.items().get(self.pcur).copied() else {
            return;
        };
        let w = &mut plan.tasks[ti].workers[wi];
        if harness {
            let ids: Vec<&str> = catalog.iter().map(|h| h.id.as_str()).collect();
            let idx = ids.iter().position(|h| *h == w.harness).map(|i| i as i32).unwrap_or(-1);
            let n = ids.len() as i32;
            if n == 0 {
                return;
            }
            let next = (idx + dir).rem_euclid(n) as usize;
            w.harness = ids[next].to_string();
            let models = &catalog[next].models;
            if !models.contains(&w.model)
                && let Some(m) = models.first()
            {
                w.model = m.clone();
            }
        } else {
            let models: Vec<String> = catalog
                .iter()
                .find(|h| h.id == w.harness)
                .map(|h| h.models.clone())
                .unwrap_or_else(|| catalog.first().map(|h| h.models.clone()).unwrap_or_default());
            let n = models.len() as i32;
            if n == 0 {
                return;
            }
            let idx = models.iter().position(|m| *m == w.model).map(|i| i as i32).unwrap_or(-1);
            w.model = models[(idx + dir).rem_euclid(n) as usize].clone();
        }
        self.plan_dirty = true;
    }

    fn start_cred_edit(&mut self) {
        self.creds.before_edit = self.data.creds.get(self.creds.csel).cloned();
        self.mode = Mode::Insert;
    }

    fn cancel_cred_edit(&mut self) {
        let csel = self.creds.csel;
        match self.creds.before_edit.take() {
            Some(before) if before.is_new => {
                if csel < self.data.creds.len() {
                    self.data.creds.remove(csel);
                }
                self.creds.csel = csel.min(self.data.creds.len().saturating_sub(1));
                self.creds.cfield = 0;
            }
            Some(before) => {
                if let Some(c) = self.data.creds.get_mut(csel) {
                    *c = before;
                }
            }
            None => {}
        }
    }

    fn edit_cred(&mut self, f: impl FnOnce(&mut String)) {
        let (csel, cfield) = (self.creds.csel, self.creds.cfield);
        if let Some(field) = self.data.creds.get_mut(csel).and_then(|c| c.fields.get_mut(cfield)) {
            f(&mut field.value);
        }
        self.sync_new_cred_name();
    }

    /// A new credential takes its name from the provider field, and grows a
    /// base-url field when the provider is LiteLLM.
    fn sync_new_cred_name(&mut self) {
        let Some(c) = self.data.creds.get_mut(self.creds.csel) else {
            return;
        };
        if !c.is_new {
            return;
        }
        let provider = c.fields.first().map(|f| f.value.trim().to_string()).unwrap_or_default();
        c.name = if provider.is_empty() { "New credential".into() } else { provider.clone() };
        let litellm = provider.to_lowercase().contains("litellm");
        let has_url = c.fields.iter().any(|f| f.label == "base url");
        if litellm && !has_url {
            c.fields.insert(1, CredField::new("base url", "", false));
            c.kind_label = "URL + key".into();
        } else if !litellm && has_url && self.creds.cfield != 1 {
            c.fields.retain(|f| f.label != "base url");
            c.kind_label = "API key".into();
        }
    }

    fn test_cred(&mut self) -> Vec<Cmd> {
        let Some(c) = self.data.creds.get(self.creds.csel) else {
            return vec![];
        };
        if c.is_new {
            self.flash(FlashMsg::new("! save the credential first · ⏎ to edit, ⏎ again to save", Tone::HUMAN));
            return vec![];
        }
        self.creds.testing = true;
        vec![Cmd::TestCredential(c.key.clone())]
    }

    fn add_cred(&mut self) {
        let n = self.data.creds.iter().filter(|c| c.is_new).count();
        self.data.creds.push(CredView {
            key: format!("new-{n}"),
            name: "New credential".into(),
            kind_label: "API key".into(),
            fields: vec![CredField::new("provider", "", false), CredField::new("api key", "", true)],
            status_text: "not tested".into(),
            status: Status::Queued,
            is_new: true,
        });
        self.creds.csel = self.data.creds.len() - 1;
        self.creds.cfield = 0;
        self.creds.before_edit = self.data.creds.last().cloned();
        self.mode = Mode::Insert;
    }

    pub fn run_action(&mut self, a: PalAction) -> Vec<Cmd> {
        match a {
            PalAction::Go(s) => self.go(s),
            PalAction::Approve => {
                let cmds = self.go(Screen::Plans);
                if self.data.plan.is_some() {
                    self.overlay = Some(Overlay::Confirm);
                }
                cmds
            }
            PalAction::Retry => self.task_cmd(Cmd::Retry),
            PalAction::Stop => self.task_cmd(Cmd::Stop),
            PalAction::Escalate => self.task_cmd(Cmd::Escalate),
            PalAction::Feedback => {
                let cmds = self.go(Screen::Run);
                self.overlay = Some(Overlay::Feedback { buf: String::new() });
                cmds
            }
            PalAction::ToggleFollow => {
                let cmds = self.go(Screen::Logs);
                self.logs.follow = !self.logs.follow;
                self.logs.frozen = self.log_total();
                cmds
            }
            PalAction::TestCred(key) => {
                let cmds = self.go(Screen::Creds);
                if let Some(i) = self.data.creds.iter().position(|c| c.key == key) {
                    self.creds.csel = i;
                    self.creds.cfield = 0;
                }
                let mut cmds = cmds;
                cmds.extend(self.test_cred());
                cmds
            }
            PalAction::OpenRun(key) => self.open_run(key),
            PalAction::OpenPlan(key) => {
                let mut cmds = self.go(Screen::Plans);
                cmds.push(Cmd::OpenPlan(key));
                cmds
            }
            PalAction::OpenThread(key) => {
                let mut cmds = self.go(Screen::Chat);
                cmds.push(Cmd::OpenThread(key));
                cmds
            }
            PalAction::NewIdea => self.new_thread(),
            PalAction::Pair => {
                self.overlay = Some(Overlay::Pair(None));
                vec![Cmd::PairStart]
            }
            PalAction::SignOut => self.sign_out(),
            PalAction::Quit => {
                self.quit = true;
                vec![Cmd::Quit]
            }
        }
    }
}

fn f_owned(s: &str) -> String {
    s.trim().to_string()
}

/// Harness/model lists from the prototype, used until the daemon's
/// `/harnesses` catalog arrives (and by demo mode).
pub fn default_catalog() -> Vec<HarnessOption> {
    let models: Vec<String> = ["claude-sonnet-4.5", "claude-opus-4.1", "gpt-5", "gpt-5-codex", "qwen3-coder"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    [("claude_sdk", "Claude Agent SDK"), ("codex", "Codex"), ("opencode", "OpenCode")]
        .iter()
        .map(|(id, label)| HarnessOption { id: id.to_string(), label: label.to_string(), models: models.clone() })
        .collect()
}
