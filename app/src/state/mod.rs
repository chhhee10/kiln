//! Shared reactive app state for every surface (desktop now, Android next).
//!
//! # How a surface consumes state
//!
//! 1. `lib.rs` creates one [`AppState`] with [`AppState::new`], calls
//!    [`AppState::boot`] and `provide_context(app)`. Any component gets it
//!    back with [`use_app()`]. `AppState` is `Copy` (it is a bundle of
//!    arena-allocated signals), so move it into closures freely.
//! 2. **Read** domain data from `app.data` (an `RwSignal<data::Data>`). Never
//!    clone the whole thing in a view; derive what you need in a `Memo` so the
//!    view only re-renders when that slice changes:
//!    ```ignore
//!    let cols = Memo::new(move |_| app.data.with(|d| derive::board_columns(d, &app.board_filter.get())));
//!    ```
//!    `state::derive` already has the board, timeline, memory, palette and
//!    credential view-models; `state::util` has the formatters. Add new pure
//!    helpers there (with tests) rather than computing in views.
//! 3. **Write** only through the action methods in `api.rs`
//!    (`app.task_action`, `app.send_message`, `app.approve_plan`,
//!    `app.update_memory`, `app.decide_memory`, `app.update_settings`,
//!    `app.pair_start`, `app.login`, …). They talk to the daemon (or the demo
//!    simulation), apply the result to `app.data` and show toasts. Live
//!    WebSocket events are applied automatically (`live.rs`, with reconnect +
//!    backoff + resync), so surfaces never touch the socket.
//! 4. **Navigate** with `app.go(Route::…)`; read `app.route`. Hash routes are
//!    shared by all surfaces (`#/task/FP-142` deep links work on Android too).
//!    `app.auth` says whether to show login / setup / the app.
//! 5. UI-only state that both surfaces need lives here too: `toast`,
//!    `selected_task`, `board_filter`, `palette_open`, `feedback_open`, … A
//!    surface may ignore the ones it has no UI for. Surface-local state
//!    (e.g. an Android bottom sheet) belongs in the surface module.
//! 6. `app.tick` increments every second (spinners, "edited just now"); read
//!    it inside a memo to make it time-dependent.
//! 7. `app.is_demo()` is true for the offline demo (`?demo=1` / "Try demo").
//!    The demo data and simulation produce the same `ServerEvent`s as the
//!    daemon, so a surface needs no demo-specific code.

pub mod api;
pub mod data;
pub mod demo;
pub mod derive;
pub mod keys;
#[cfg(target_arch = "wasm32")]
pub mod live;
pub mod platform;
pub mod route;
pub mod util;

use data::Data;
use derive::{BoardFilter, BrainFilter};
use kiln_api_client::Client;
use kiln_types::ids::*;
use kiln_types::status::WorkerRole;
use leptos::prelude::*;
pub use route::{Route, Screen, SettingsTab};

/// Which top-level screen family to show.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthState {
    /// Probing the daemon.
    Booting,
    /// No daemon at `base` (or incompatible). Login shows the error, a server
    /// field and "Try demo".
    Unreachable {
        base: String,
        error: String,
    },
    /// `/info` says no users exist yet: first-run setup.
    Setup,
    SignedOut,
    SignedIn,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub message: String,
    pub ok: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

/// Live connection health, shown by surfaces that want it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    Offline,
    Connecting,
    Live,
    Reconnecting,
}

/// Daemon reachability for the login screen ("mac-mini.tailnet · reachable · 18ms").
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Reach {
    pub host: String,
    pub latency_ms: Option<u32>,
    pub ok: bool,
}

/// What the lightbox shows: an artifact image/video URL, or a demo placeholder.
#[derive(Clone, Debug, PartialEq)]
pub struct Lightbox {
    pub title: String,
    pub url: Option<String>,
    pub video: bool,
}

/// Modals other than feedback (which has its own flag for the shortcut).
#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    CreateAccount,
    AddCredential,
    RotateCredential(CredentialId),
}

#[derive(Clone)]
pub enum Backend {
    None,
    Live(Client),
    Demo,
}

#[cfg(target_arch = "wasm32")]
pub type WsConn = kiln_api_client::ws::Connection;
#[cfg(not(target_arch = "wasm32"))]
pub type WsConn = ();

#[derive(Clone, Copy)]
pub struct AppState {
    pub data: RwSignal<Data>,
    pub auth: RwSignal<AuthState>,
    pub route: RwSignal<Route>,
    pub reach: RwSignal<Reach>,
    pub conn: RwSignal<Conn>,
    pub toast: RwSignal<Option<Toast>>,
    pub tick: RwSignal<u64>,
    pub theme: RwSignal<Theme>,
    // ---- shared UI state
    pub board_filter: RwSignal<BoardFilter>,
    /// Task shown in the board side panel (peek).
    pub peek: RwSignal<Option<TaskId>>,
    /// The "current" task: peek or full page; palette task commands act on it.
    pub selected_task: RwSignal<Option<TaskId>>,
    /// Log tab override (None = follow the current stage).
    pub log_tab: RwSignal<Option<WorkerRole>>,
    pub palette_open: RwSignal<bool>,
    pub feedback_open: RwSignal<bool>,
    pub lightbox: RwSignal<Option<Lightbox>>,
    pub modal: RwSignal<Option<Modal>>,
    pub brain_filter: RwSignal<BrainFilter>,
    pub brain_query: RwSignal<String>,
    pub brain_selected: RwSignal<Option<MemoryId>>,
    /// Project for a new (not yet created) planner thread.
    pub new_idea_project: RwSignal<Option<ProjectId>>,
    /// Plan with unsaved local edits → pending save (debounced).
    pub plan_dirty: RwSignal<Option<PlanId>>,
    /// Loop limit stepper in Settings (client-side until the API has it).
    pub loop_limit: RwSignal<u32>,
    // ---- non-reactive internals
    pub(crate) backend: StoredValue<Backend, LocalStorage>,
    pub(crate) sim: StoredValue<Option<demo::DemoSim>, LocalStorage>,
    pub(crate) ws: StoredValue<Option<WsConn>, LocalStorage>,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) ws_attempt: StoredValue<u32, LocalStorage>,
    pub(crate) followed: StoredValue<Vec<TaskId>, LocalStorage>,
    pub(crate) toast_seq: StoredValue<u64, LocalStorage>,
    pub keys: StoredValue<keys::KeyState, LocalStorage>,
}

pub fn use_app() -> AppState {
    expect_context::<AppState>()
}

pub const STORAGE_SERVER: &str = "kiln.server";
pub const STORAGE_TOKEN: &str = "kiln.token";
pub const STORAGE_THEME: &str = "kiln.theme";
pub const STORAGE_LOOP: &str = "kiln.loop_limit";

impl AppState {
    pub fn new() -> Self {
        let theme = match platform::local_get(STORAGE_THEME).as_deref() {
            Some("light") => Theme::Light,
            _ => Theme::Dark,
        };
        let loop_limit = platform::local_get(STORAGE_LOOP)
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        Self {
            data: RwSignal::new(Data::default()),
            auth: RwSignal::new(AuthState::Booting),
            route: RwSignal::new(Route::parse(&platform::get_hash()).unwrap_or(Route::Board)),
            reach: RwSignal::new(Reach::default()),
            conn: RwSignal::new(Conn::Offline),
            toast: RwSignal::new(None),
            tick: RwSignal::new(0),
            theme: RwSignal::new(theme),
            board_filter: RwSignal::new(BoardFilter::default()),
            peek: RwSignal::new(None),
            selected_task: RwSignal::new(None),
            log_tab: RwSignal::new(None),
            palette_open: RwSignal::new(false),
            feedback_open: RwSignal::new(false),
            lightbox: RwSignal::new(None),
            modal: RwSignal::new(None),
            brain_filter: RwSignal::new(BrainFilter::All),
            brain_query: RwSignal::new(String::new()),
            brain_selected: RwSignal::new(None),
            new_idea_project: RwSignal::new(None),
            plan_dirty: RwSignal::new(None),
            loop_limit: RwSignal::new(loop_limit),
            backend: StoredValue::new_local(Backend::None),
            sim: StoredValue::new_local(None),
            ws: StoredValue::new_local(None),
            ws_attempt: StoredValue::new_local(0),
            followed: StoredValue::new_local(Vec::new()),
            toast_seq: StoredValue::new_local(0),
            keys: StoredValue::new_local(keys::KeyState::default()),
        }
    }

    pub fn is_demo(&self) -> bool {
        self.backend.with_value(|b| matches!(b, Backend::Demo))
    }

    pub fn client(&self) -> Option<Client> {
        self.backend.with_value(|b| match b {
            Backend::Live(c) => Some(c.clone()),
            _ => None,
        })
    }

    /// Untracked; views should derive `is_admin` in a memo instead.
    pub fn is_admin(&self) -> bool {
        self.data.with_untracked(|d| d.is_admin())
    }

    /// Show a toast for 2.6 s (one at a time, newest wins).
    pub fn toast(&self, message: impl Into<String>, ok: bool) {
        let id = self.toast_seq.with_value(|v| *v) + 1;
        self.toast_seq.set_value(id);
        self.toast.set(Some(Toast {
            id,
            message: message.into(),
            ok,
        }));
        let toast = self.toast;
        set_timeout(
            move || {
                if toast.get_untracked().is_some_and(|t| t.id == id) {
                    toast.set(None);
                }
            },
            std::time::Duration::from_millis(2600),
        );
    }

    pub fn set_theme(&self, t: Theme) {
        self.theme.set(t);
        platform::set_root_attr("data-theme", t.as_str());
        platform::local_set(STORAGE_THEME, t.as_str());
    }

    pub fn set_loop_limit(&self, v: u32) {
        let v = v.clamp(1, 5);
        self.loop_limit.set(v);
        platform::local_set(STORAGE_LOOP, &v.to_string());
    }

    /// Navigate (updates the hash so back/forward and deep links work).
    pub fn go(&self, r: Route) {
        self.palette_open.set(false);
        if let Route::Task(key) = &r
            && let Some(t) = self
                .data
                .with_untracked(|d| d.task_by_key(key).map(|t| t.id))
        {
            self.select_task(t);
        }
        if r.screen() != Screen::Board {
            self.peek.set(None);
        }
        platform::set_hash(&r.to_hash());
        self.route.set(r.clone());
        self.on_route(&r);
    }

    /// Called on navigation and on `hashchange`.
    pub fn on_route(&self, r: &Route) {
        match r {
            Route::Task(key) => {
                if let Some(t) = self
                    .data
                    .with_untracked(|d| d.task_by_key(key).map(|t| t.id))
                {
                    self.select_task(t);
                }
            }
            Route::Planner(t) => {
                let id = t.or_else(|| {
                    self.data
                        .with_untracked(|d| d.threads.first().map(|t| t.id))
                });
                if let Some(id) = id {
                    self.open_thread(id);
                }
            }
            Route::Admin => self.refresh_admin(),
            _ => {}
        }
    }

    /// Make `id` the current task: loads its detail + logs and follows it live.
    pub fn select_task(&self, id: TaskId) {
        if self.selected_task.get_untracked() != Some(id) {
            self.log_tab.set(None);
        }
        self.selected_task.set(Some(id));
        self.sim.update_value(|s| {
            if let Some(s) = s {
                s.selected = Some(id);
            }
        });
        self.load_task(id);
    }

    pub fn open_peek(&self, id: TaskId) {
        self.peek.set(Some(id));
        self.select_task(id);
    }

    /// The task the panel / task page shows (falls back to the first task).
    pub fn current_task_id(&self) -> Option<TaskId> {
        self.selected_task
            .get_untracked()
            .or_else(|| self.data.with_untracked(|d| d.tasks.first().map(|t| t.id)))
    }

    pub fn current_screen(&self) -> Screen {
        self.route.get_untracked().screen()
    }

    /// Initial boot: demo, or probe the daemon and restore the session.
    pub fn boot(&self) {
        platform::set_root_attr("data-theme", self.theme.get_untracked().as_str());
        if platform::query_param("demo").is_some_and(|v| v != "0") {
            let setup = platform::query_param("setup").is_some_and(|v| v != "0");
            self.start_demo(!setup && !platform::get_hash().is_empty());
            if setup {
                self.auth.set(AuthState::Setup);
            }
            return;
        }
        if let Some(s) = platform::query_param("server") {
            platform::local_set(STORAGE_SERVER, &s);
        }
        let base = platform::local_get(STORAGE_SERVER).unwrap_or_else(default_base);
        self.connect_to(base);
    }

    /// Switch to the offline demo. `signed_in` skips the login screen.
    pub fn start_demo(&self, signed_in: bool) {
        let now = chrono::Utc::now();
        let d = demo::seed(now);
        let sim = demo::DemoSim::new(&d);
        let first_selected = sim.selected;
        self.backend.set_value(Backend::Demo);
        self.sim.set_value(Some(sim));
        self.data.set(d);
        self.selected_task.set(first_selected);
        self.reach.set(Reach {
            host: "mac-mini.tailnet".into(),
            latency_ms: Some(18),
            ok: true,
        });
        self.conn.set(Conn::Live);
        self.auth.set(if signed_in {
            AuthState::SignedIn
        } else {
            AuthState::SignedOut
        });
        if signed_in {
            let r = self.route.get_untracked();
            self.on_route(&r);
        }
        let app = *self;
        set_interval(
            move || {
                if !app.is_demo() {
                    return;
                }
                app.tick.update(|t| *t += 1);
                app.demo_step();
            },
            std::time::Duration::from_millis(1000),
        );
    }

    fn demo_step(&self) {
        let now = chrono::Utc::now();
        let evs = self
            .sim
            .try_update_value(|s| {
                s.as_mut()
                    .map(|s| self.data.with_untracked(|d| s.step(d, now)))
            })
            .flatten()
            .unwrap_or_default();
        let tick = self.tick.get_untracked();
        self.data.update(|d| {
            for e in &evs {
                d.apply(e);
            }
            demo::refresh_admin(d, tick);
        });
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// The page origin when served over http(s) (the daemon's `/app`); in the
/// Tauri WebView (`tauri://localhost`) there is no usable default, so the
/// probe fails fast and the login screen asks for the server URL.
///
/// Note: the api-client documents `""` as "same origin", but reqwest on wasm
/// rejects relative URLs ("builder error"), so pass the absolute origin.
fn default_base() -> String {
    let origin = platform::origin();
    if !platform::is_tauri() && (origin.starts_with("http://") || origin.starts_with("https://")) {
        origin
    } else {
        "http://127.0.0.1:7420".into()
    }
}
