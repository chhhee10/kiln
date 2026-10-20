//! Android surface: "Android v2", chat-first Material 3.
//!
//! - `nav`    tabs, pushed screens, sheets, back stack, deep links (pure).
//! - `feed`   the Planner feed view-model: cards, chips, resolution (pure).
//! - `bridge` Tauri IPC (kiln-mobile plugin), Web Speech, mic level.
//! - screens: `planner`, `runs`, `you`, `task`, `plan`, overlays in
//!   `sheets`, `voice`, sign-in in `login`.
//!
//! Shared data, actions and demo mode come from `crate::state` (see the doc
//! comment at the top of `state/mod.rs`). Everything here is surface-local
//! UI state, kept in the [`Ui`] context.

pub mod bridge;
pub mod feed;
pub mod nav;

mod login;
mod plan;
mod planner;
mod runs;
mod sheets;
mod task;
mod voice;
mod you;

use crate::state::*;
use chrono::Utc;
use feed::{CardKey, FeedItem, FeedLocal, Filter};
use kiln_types::ids::*;
use kiln_types::{Notification, NotificationKind};
use leptos::prelude::*;
use nav::{DeepLink, NavState, Pushed, Tab};
use std::time::Duration;

/// The in-app heads-up notification.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadsUp {
    pub seq: u64,
    pub glyph: &'static str,
    pub color: &'static str,
    pub title: String,
    pub body: String,
    pub link: DeepLink,
    pub notification: Option<NotificationId>,
}

/// Native push registration state (You screen).
#[derive(Clone, Debug, PartialEq, Default)]
pub enum PushState {
    /// Not in the Android shell (browser): in-app notifications only.
    #[default]
    InApp,
    Registering,
    Registered(String),
    /// No UnifiedPush distributor (e.g. ntfy) installed, or it failed.
    Unavailable(String),
}

/// Android-only UI state shared by the screens.
#[derive(Clone, Copy)]
pub struct Ui {
    pub app: AppState,
    pub nav: RwSignal<NavState>,
    pub filter: RwSignal<Filter>,
    pub local: RwSignal<FeedLocal>,
    /// Conversation shown on the home screen (None = most recent).
    pub thread: RwSignal<Option<ThreadId>>,
    /// Card with the 2px primary ring (deep link target), for 2.2 s.
    pub highlight: RwSignal<Option<CardKey>>,
    /// Ask the feed to scroll a card into view.
    pub scroll_req: RwSignal<Option<CardKey>>,
    pub heads: RwSignal<Option<HeadsUp>>,
    pub fb_text: RwSignal<String>,
    /// Task the feedback draft belongs to (switching tasks clears it).
    pub fb_task: RwSignal<Option<TaskId>>,
    pub push: RwSignal<PushState>,
    /// The assembled feed (unfiltered).
    pub items: Memo<Vec<FeedItem>>,
    seq: StoredValue<u64, LocalStorage>,
    demo_push: StoredValue<usize, LocalStorage>,
}

pub fn use_ui() -> Ui {
    expect_context::<Ui>()
}

impl Ui {
    fn next_seq(&self) -> u64 {
        let n = self.seq.get_value() + 1;
        self.seq.set_value(n);
        n
    }

    /// M3 snackbar (rides on the shared toast).
    pub fn snack(&self, msg: impl Into<String>) {
        self.app.toast(msg, true);
    }

    pub fn task_id(&self, key: &str) -> Option<TaskId> {
        self.app
            .data
            .with_untracked(|d| d.task_by_key(key).map(|t| t.id))
    }

    /// Push the task detail screen (and load / follow the task).
    pub fn open_task(&self, key: &str) {
        if let Some(id) = self.task_id(key) {
            self.app.select_task(id);
        }
        self.nav.update(|n| n.push(Pushed::Task(key.to_string())));
    }

    pub fn open_task_id(&self, id: TaskId) {
        if let Some(k) = self
            .app
            .data
            .with_untracked(|d| d.task(id).map(|t| t.key.clone()))
        {
            self.open_task(&k);
        }
    }

    /// Resolve a card ("✓ text") and optionally post a planner follow-up.
    pub fn resolve(&self, item: &FeedItem, text: String, followup: Option<String>) {
        self.local.update(|l| l.resolve(item, text));
        if let Some(f) = followup {
            let local = self.local;
            set_timeout(
                move || local.update(|l| l.say(f, Utc::now())),
                Duration::from_millis(500),
            );
        }
    }

    /// Scroll the feed to a card and ring it for 2.2 s.
    pub fn highlight(&self, key: CardKey) {
        let seq = self.next_seq();
        self.scroll_req.set(Some(key.clone()));
        self.highlight.set(Some(key));
        let (hl, s) = (self.highlight, self.seq);
        set_timeout(
            move || {
                if s.get_value() == seq {
                    hl.set(None);
                }
            },
            Duration::from_millis(2200),
        );
    }

    /// Follow a deep link (notification tap, kiln:// URL, hash).
    pub fn go_link(&self, link: DeepLink) {
        if let DeepLink::Pair { url, code } = &link {
            let app = self.app;
            app.pair_complete(url.clone(), code.clone(), move |e| {
                app.toast(format!("Pairing failed · {e}"), false)
            });
            return;
        }
        let fb = match &link {
            DeepLink::Task {
                key,
                feedback: true,
            } => self.task_id(key),
            _ => None,
        };
        if let DeepLink::Task { key, .. } = &link
            && let Some(id) = self.task_id(key)
        {
            self.app.select_task(id);
        }
        let hl = self.nav.try_update(|n| n.apply(&link, fb)).flatten();
        if let Some(k) = hl {
            self.filter.set(Filter::All);
            self.highlight(k);
        }
    }

    /// Show the heads-up card for 5 s.
    pub fn heads_up(&self, mut h: HeadsUp) {
        h.seq = self.next_seq();
        let seq = h.seq;
        self.heads.set(Some(h));
        self.nav.update(|n| n.heads_up = true);
        let (heads, nav) = (self.heads, self.nav);
        set_timeout(
            move || {
                if heads.get_untracked().is_some_and(|x| x.seq == seq) {
                    heads.set(None);
                    nav.update(|n| n.heads_up = false);
                }
            },
            Duration::from_millis(5000),
        );
    }

    /// Demo only: simulate a push (prototype "Simulate push").
    pub fn simulate_push(&self) {
        let i = self.demo_push.get_value();
        self.demo_push.set_value(i + 1);
        let Some(me) = self
            .app
            .data
            .with_untracked(|d| d.me.as_ref().map(|u| u.id))
        else {
            return;
        };
        let task = |k: &str| self.task_id(k);
        let plan = self.app.data.with_untracked(|d| {
            d.plans
                .values()
                .filter(|p| p.status == kiln_types::PlanStatus::Drafting && p.owner_id == me)
                .max_by_key(|p| p.updated_at)
                .map(|p| (p.id, p.spec.feature.clone(), p.spec.tasks.len()))
        });
        let (kind, title, body, link, task_id, plan_id) = match i % 4 {
            0 => (
                NotificationKind::TaskFailed,
                "FP-145 typecheck failed".to_string(),
                "Builder looping back · attempt 2/3".to_string(),
                "task/FP-145".to_string(),
                task("FP-145"),
                None,
            ),
            1 => match plan {
                Some((id, name, n)) => (
                    NotificationKind::PlanReady,
                    "Plan ready to approve".to_string(),
                    format!("{name} · {n} tasks"),
                    format!("plan/{id}"),
                    None,
                    Some(id),
                ),
                None => (
                    NotificationKind::TaskReady,
                    "2 PRs ready for review".into(),
                    "#433 Blog pagination · #431 Rate-limit headers".into(),
                    "chat/prs".into(),
                    None,
                    None,
                ),
            },
            2 => (
                NotificationKind::TaskEscalated,
                "FP-136 needs your call".to_string(),
                "QA and Reviewer disagree on focus order".to_string(),
                "task/FP-136".to_string(),
                task("FP-136"),
                None,
            ),
            _ => (
                NotificationKind::TaskReady,
                "2 PRs ready for review".to_string(),
                "#433 Blog pagination · #431 Rate-limit headers".to_string(),
                "chat/prs".to_string(),
                None,
                None,
            ),
        };
        let n = Notification {
            id: NotificationId::new(),
            user_id: me,
            kind,
            title,
            body,
            deep_link: link,
            actions: vec![],
            task_id,
            plan_id,
            memory_id: None,
            read: false,
            resolved: None,
            created_at: Utc::now(),
        };
        self.app
            .apply_events(vec![kiln_types::event::ServerEvent::Notification {
                notification: n,
            }]);
    }
}

/// Glyph + status color of a notification (heads-up icon, design table).
pub fn notification_glyph(k: NotificationKind) -> (&'static str, &'static str) {
    match k {
        NotificationKind::PlanReady => ("◇", "#cdf04a"),
        NotificationKind::TaskFailed => ("✕", "#f0605a"),
        NotificationKind::TaskEscalated => ("!", "#f2c14e"),
        NotificationKind::TaskReady => ("✓", "#4fd19a"),
        NotificationKind::MemoryApproval => ("◑", "#dde6c8"),
        NotificationKind::WorkersLive => ("◐", "#5fa8f0"),
        NotificationKind::SystemAlert => ("!", "#f0605a"),
    }
}

fn channel_key(k: NotificationKind) -> &'static str {
    match k.channel() {
        kiln_types::NotificationChannel::Decisions => "decisions",
        kiln_types::NotificationChannel::Failures => "failures",
        kiln_types::NotificationChannel::ReadyForReview => "ready_for_review",
        kiln_types::NotificationChannel::LiveWorkers => "live_workers",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AuthKind {
    Booting,
    Login,
    Setup,
    App,
}

#[component]
pub fn AndroidApp() -> impl IntoView {
    let app = use_app();
    let nav = RwSignal::new(NavState::default());
    let local = RwSignal::new(FeedLocal::default());
    let thread = RwSignal::new(None::<ThreadId>);
    let items = Memo::new(move |_| {
        let tick = app.tick.get();
        let now = Utc::now();
        let pick = thread.get();
        app.data
            .with(|d| local.with(|l| feed::assemble(d, pick, l, now, tick)))
    });
    let ui = Ui {
        app,
        nav,
        filter: RwSignal::new(Filter::All),
        local,
        thread,
        highlight: RwSignal::new(None),
        scroll_req: RwSignal::new(None),
        heads: RwSignal::new(None),
        fb_text: RwSignal::new(String::new()),
        fb_task: RwSignal::new(None),
        push: RwSignal::new(PushState::default()),
        items,
        seq: StoredValue::new_local(0),
        demo_push: StoredValue::new_local(0),
    };
    provide_context(ui);

    let kind = Memo::new(move |_| match app.auth.get() {
        AuthState::Booting => AuthKind::Booting,
        AuthState::Unreachable { .. } | AuthState::SignedOut => AuthKind::Login,
        AuthState::Setup => AuthKind::Setup,
        AuthState::SignedIn => AuthKind::App,
    });

    // ---- feed bookkeeping: first-seen times, cards resolved elsewhere.
    let prev = StoredValue::new_local(Vec::<FeedItem>::new());
    Effect::new(move |_| {
        let its = items.get();
        if kind.get_untracked() != AuthKind::App {
            return;
        }
        let now = Utc::now();
        let mut l = local.get_untracked();
        let mut changed = false;
        if l.started.is_none() {
            l.started = Some(now);
            changed = true;
        }
        changed |= l.register(&its, now);
        if !l.primed && app.data.with_untracked(|d| !d.tasks.is_empty()) {
            l.primed = true;
            changed = true;
        }
        changed |= prev.with_value(|p| app.data.with_untracked(|d| l.reconcile(p, &its, d)));
        prev.set_value(its);
        if changed {
            local.set(l);
        }
    });
    Effect::new(move |_| {
        if kind.get() == AuthKind::App {
            set_timeout(
                move || {
                    if !local.with_untracked(|l| l.primed) {
                        local.update(|l| l.primed = true);
                    }
                },
                Duration::from_millis(4000),
            );
        }
    });

    // ---- load the home conversation.
    let loaded = StoredValue::new_local(None::<ThreadId>);
    Effect::new(move |_| {
        if kind.get() != AuthKind::App {
            return;
        }
        let pick = thread.get();
        let tid = app.data.with(|d| feed::current_thread(d, pick));
        if let Some(t) = tid
            && loaded.get_value() != Some(t)
        {
            loaded.set_value(Some(t));
            app.open_thread(t);
        }
    });

    // ---- startup deep link from the URL hash (#/task/FP-142, …).
    let initial = StoredValue::new_local(false);
    Effect::new(move |_| {
        if kind.get() != AuthKind::App || initial.get_value() {
            return;
        }
        if app.data.with(|d| d.tasks.is_empty()) && !app.is_demo() {
            return;
        }
        initial.set_value(true);
        if let Some(link) = nav::parse_deep_link(&platform::get_hash())
            && link != DeepLink::Chat(None)
        {
            ui.go_link(link);
        }
        if app.is_demo()
            && let Some(i) = platform::query_param("push").and_then(|v| v.parse::<usize>().ok())
        {
            ui.demo_push.set_value(i);
            set_timeout(move || ui.simulate_push(), Duration::from_millis(1200));
        }
    });

    // ---- heads-up for new notifications.
    let known = StoredValue::new_local(None::<std::collections::HashSet<NotificationId>>);
    Effect::new(move |_| {
        let notes: Vec<Notification> = app.data.with(|d| d.notifications.clone());
        if kind.get() != AuthKind::App {
            return;
        }
        let first = known.with_value(|k| k.is_none());
        let fresh: Vec<Notification> = known
            .try_update_value(|k| {
                let k = k.get_or_insert_with(Default::default);
                notes.iter().filter(|n| k.insert(n.id)).cloned().collect()
            })
            .unwrap_or_default();
        if first {
            return;
        }
        let enabled = |n: &Notification| {
            app.data.with_untracked(|d| {
                d.settings
                    .as_ref()
                    .and_then(|s| s.notification_channels.get(channel_key(n.kind)).copied())
                    .unwrap_or(true)
            })
        };
        if let Some(n) = fresh
            .into_iter()
            .filter(|n| !n.read && n.resolved.is_none())
            .filter(|n| n.kind != NotificationKind::WorkersLive)
            .find(|n| enabled(n))
        {
            let (glyph, color) = notification_glyph(n.kind);
            ui.heads_up(HeadsUp {
                seq: 0,
                glyph,
                color,
                title: n.title.clone(),
                body: n.body.clone(),
                link: nav::link_for_notification(&n),
                notification: Some(n.id),
            });
        }
    });

    install_back_handling(ui);
    install_shell_bridges(ui, kind);

    view! {
        <div class="a-root">
            {move || match kind.get() {
                AuthKind::Booting => view! { <div class="a-boot"><div class="a-brand lg"><span></span></div></div> }.into_any(),
                AuthKind::Login => view! { <login::Login/> }.into_any(),
                AuthKind::Setup => view! { <login::SetupNeeded/> }.into_any(),
                AuthKind::App => view! { <Shell/> }.into_any(),
            }}
            <Snackbar/>
        </div>
    }
}

/// Screens + navigation bar + overlays.
#[component]
fn Shell() -> impl IntoView {
    let ui = use_ui();
    let screen = Memo::new(move |_| ui.nav.with(|n| (n.tab, n.top().cloned())));
    let show_nav = Memo::new(move |_| ui.nav.with(|n| n.show_nav_bar()));
    view! {
        <div class="a-screens">
            {move || match screen.get() {
                (_, Some(Pushed::Task(k))) => view! { <task::TaskScreen key=k/> }.into_any(),
                (_, Some(Pushed::PlanReview(p))) => view! { <plan::PlanReview plan=p/> }.into_any(),
                (Tab::Planner, None) => view! { <planner::PlannerHome/> }.into_any(),
                (Tab::Runs, None) => view! { <runs::Runs/> }.into_any(),
                (Tab::You, None) => view! { <you::You/> }.into_any(),
            }}
        </div>
        <Show when=move || show_nav.get()>
            <NavBar/>
        </Show>
        <sheets::Sheets/>
        <Show when=move || ui.nav.with(|n| n.voice)>
            <voice::VoiceOverlay/>
        </Show>
        <HeadsUpCard/>
    }
}

#[component]
fn NavBar() -> impl IntoView {
    let ui = use_ui();
    let badge = Memo::new(move |_| ui.items.with(|i| feed::counts(i).needs));
    view! {
        <nav class="a-navbar">
            {Tab::ALL.into_iter().map(|t| {
                let on = move || ui.nav.with(|n| n.tab == t);
                view! {
                    <button class="a-dest" class:on=on on:click=move |_| ui.nav.update(|n| n.tab_to(t))>
                        <span class="a-dest-pill">
                            <span class="g">{t.glyph()}</span>
                            {(t == Tab::Planner).then(|| view! {
                                <Show when=move || { badge.get() > 0 }>
                                    <span class="a-badge">{move || badge.get()}</span>
                                </Show>
                            })}
                        </span>
                        <span class="a-dest-label">{t.label()}</span>
                    </button>
                }
            }).collect_view()}
        </nav>
    }
}

#[component]
fn HeadsUpCard() -> impl IntoView {
    let ui = use_ui();
    let shown = move || {
        ui.nav
            .with(|n| n.heads_up)
            .then(|| ui.heads.get())
            .flatten()
    };
    view! {
        {move || shown().map(|h| {
            let link = h.link.clone();
            let nid = h.notification;
            view! {
                <div class="a-heads" on:click=move |_| {
                    ui.heads.set(None);
                    if let Some(id) = nid {
                        ui.app.data.update(|d| {
                            if let Some(n) = d.notifications.iter_mut().find(|n| n.id == id) {
                                n.read = true;
                            }
                        });
                    }
                    ui.go_link(link.clone());
                }>
                    <div class="a-heads-icon" style=format!("color:{}", h.color)>{h.glyph}</div>
                    <div class="a-heads-text">
                        <span class="meta">"Kiln · now"</span>
                        <span class="title">{h.title.clone()}</span>
                        <span class="body">{h.body.clone()}</span>
                    </div>
                </div>
            }
        })}
    }
}

#[component]
fn Snackbar() -> impl IntoView {
    let app = use_app();
    let ui = use_ui();
    let bottom = move || {
        ui.nav.with(|n| match (n.top(), n.tab) {
            (Some(Pushed::PlanReview(_)), _) => 16,
            (Some(Pushed::Task(_)), _) => 96,
            (None, Tab::Planner) => 156,
            (None, _) => 96,
        })
    };
    let visible = move || {
        app.toast.get().filter(|t| {
            // Notifications arrive as heads-up cards, not snackbars.
            !app.data
                .with(|d| d.notifications.iter().any(|n| n.title == t.message))
        })
    };
    view! {
        {move || visible().map(|t| view! {
            <div class="a-snack" class:err=!t.ok style=format!("bottom:calc({}px + var(--a-safe-bottom))", bottom())>{t.message.clone()}</div>
        })}
    }
}

/// System back: Esc and the browser/WebView history (Android back gesture
/// in the Tauri shell triggers `history.back()`). One guard history entry
/// exists whenever there is something to pop; popping it pops one layer.
fn install_back_handling(ui: Ui) {
    let has_guard = StoredValue::new_local(false);
    let ignore = StoredValue::new_local(false);
    Effect::new(move |_| {
        let root = ui.nav.with(|n| n.is_root());
        if !root && !has_guard.get_value() {
            bridge::push_guard();
            has_guard.set_value(true);
        } else if root && has_guard.get_value() {
            ignore.set_value(true);
            has_guard.set_value(false);
            bridge::history_back();
        }
    });
    #[cfg(target_arch = "wasm32")]
    {
        let h = window_event_listener(leptos::ev::popstate, move |ev| {
            if ignore.get_value() {
                ignore.set_value(false);
                return;
            }
            has_guard.set_value(bridge::state_is_guard(&ev.state()));
            ui.nav.update(|n| {
                n.back();
            });
        });
        on_cleanup(move || h.remove());
        let k = window_event_listener(leptos::ev::keydown, move |ev| {
            if ev.key() == "Escape" {
                platform::blur_active();
                ui.nav.update(|n| {
                    n.back();
                });
            }
        });
        on_cleanup(move || k.remove());
    }
}

/// Tauri Android shell: kiln:// deep links, notification taps and the
/// UnifiedPush registration (see src-tauri/ANDROID.md).
fn install_shell_bridges(ui: Ui, kind: Memo<AuthKind>) {
    if !bridge::in_tauri() {
        return;
    }
    let follow = move |v: serde_json::Value| {
        let urls: Vec<String> = match v {
            serde_json::Value::Array(a) => a
                .into_iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Object(o) => o
                .get("link")
                .or_else(|| o.get("url"))
                .and_then(|x| x.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
            _ => vec![],
        };
        for u in urls {
            if let Some(l) = nav::parse_deep_link(&u) {
                ui.go_link(l);
            }
        }
    };
    bridge::on_app_event("kiln://deep-link", follow);
    bridge::on_plugin_event("notificationTap", follow);
    bridge::invoke("take_pending_links", serde_json::json!({}), move |r| {
        if let Ok(s) = r {
            follow(serde_json::from_str(&s).unwrap_or_default());
        }
    });
    if !platform::is_android_ua() {
        return;
    }
    let app = ui.app;
    bridge::on_plugin_event("pushEndpoint", move |v| {
        let Some(ep) = v
            .get("endpoint")
            .and_then(|e| e.as_str())
            .map(str::to_string)
        else {
            return;
        };
        let key = |k: &str| v.get(k).and_then(|e| e.as_str()).map(str::to_string);
        let keys = key("p256dh").zip(key("auth"));
        let push = ui.push;
        let shown = ep.clone();
        app.register_push(ep, "Kiln Android".into(), keys, move |ok| {
            push.set(if ok {
                PushState::Registered(shown.clone())
            } else {
                PushState::Unavailable("The daemon rejected the push endpoint".into())
            })
        });
    });
    bridge::on_plugin_event("pushFailed", move |v| {
        let why = v
            .get("reason")
            .and_then(|e| e.as_str())
            .unwrap_or("registration failed")
            .to_string();
        ui.push.set(PushState::Unavailable(why));
    });
    // Register once signed in (and again after a re-login).
    Effect::new(move |_| {
        if kind.get() != AuthKind::App || app.is_demo() {
            return;
        }
        let server = app
            .client()
            .map(|c| c.base().to_string())
            .unwrap_or_default();
        let token = app.session_token().unwrap_or_default();
        ui.push.set(PushState::Registering);
        bridge::invoke(
            "plugin:kiln-mobile|register_push",
            serde_json::json!({ "server": server, "token": token, "deviceName": "Kiln Android" }),
            move |r| match r {
                Ok(s) => {
                    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or_default();
                    match v.get("status").and_then(|x| x.as_str()) {
                        Some("no_distributor") => ui.push.set(PushState::Unavailable(
                            "Install a UnifiedPush distributor (ntfy) for push".into(),
                        )),
                        Some("registered") => {
                            if let Some(ep) = v.get("endpoint").and_then(|x| x.as_str()) {
                                ui.push.set(PushState::Registered(ep.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => ui.push.set(PushState::Unavailable(e)),
            },
        );
        bridge::invoke(
            "plugin:kiln-mobile|take_pending_link",
            serde_json::json!({}),
            move |r| {
                if let Ok(s) = r {
                    follow(serde_json::from_str(&s).unwrap_or_default());
                }
            },
        );
    });
}
