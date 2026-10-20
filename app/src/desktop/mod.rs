//! Desktop surface.

pub mod admin;
pub mod board;
pub mod brain;
pub mod components;
pub mod login;
pub mod modals;
pub mod palette;
pub mod planner;
pub mod settings;
pub mod shell;
pub mod task;

use crate::state::keys::{self, KeyContext, KeyInput, Overlay, Shortcut, TaskKey};
use crate::state::*;
use kiln_types::api::TaskAction;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// Desktop-only UI signals shared between the shell, screens and overlays.
#[derive(Clone, Copy)]
pub struct DesktopUi {
    /// Bumped by ⌘↵ / ⌘⇧↵; the planner screen reacts.
    pub planner_send: RwSignal<u64>,
    pub planner_dispatch: RwSignal<u64>,
    pub palette_query: RwSignal<String>,
    pub palette_index: RwSignal<usize>,
    pub palette_run: RwSignal<u64>,
}

pub fn use_ui() -> DesktopUi {
    expect_context::<DesktopUi>()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AuthKind {
    Booting,
    Login,
    Setup,
    App,
}

/// Run a task shortcut (R/S/E/F) on the current task.
pub fn task_key(app: AppState, k: TaskKey) {
    let Some(id) = app.current_task_id() else {
        return;
    };
    match k {
        TaskKey::Retry => app.task_action(id, TaskAction::Retry),
        TaskKey::Stop => app.task_action(id, TaskAction::Stop),
        TaskKey::Escalate => app.task_action(id, TaskAction::Escalate { note: None }),
        TaskKey::Feedback => app.feedback_open.set(true),
    }
}

fn overlay(app: AppState) -> Overlay {
    if app.palette_open.get_untracked() {
        Overlay::Palette
    } else if app.feedback_open.get_untracked() || app.modal.get_untracked().is_some() {
        Overlay::Modal
    } else if app.lightbox.get_untracked().is_some() {
        Overlay::Lightbox
    } else {
        Overlay::None
    }
}

fn in_field(ev: &leptos::ev::KeyboardEvent) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
        .map(|el| {
            let tag = el.tag_name().to_lowercase();
            tag == "input" || tag == "textarea" || tag == "select" || el.is_content_editable()
        })
        .unwrap_or(false)
}

#[component]
pub fn DesktopApp() -> impl IntoView {
    let app = use_app();
    let ui = DesktopUi {
        planner_send: RwSignal::new(0),
        planner_dispatch: RwSignal::new(0),
        palette_query: RwSignal::new(String::new()),
        palette_index: RwSignal::new(0),
        palette_run: RwSignal::new(0),
    };
    provide_context(ui);

    let kind = Memo::new(move |_| match app.auth.get() {
        AuthState::Booting => AuthKind::Booting,
        AuthState::Unreachable { .. } | AuthState::SignedOut => AuthKind::Login,
        AuthState::Setup => AuthKind::Setup,
        AuthState::SignedIn => AuthKind::App,
    });

    let handle = window_event_listener(leptos::ev::keydown, move |ev| {
        let ctx = KeyContext {
            screen: app.current_screen(),
            peek_open: app.peek.get_untracked().is_some(),
            overlay: overlay(app),
            signed_in: kind.get_untracked() == AuthKind::App,
        };
        let input = KeyInput {
            key: ev.key(),
            meta: ev.meta_key(),
            ctrl: ev.ctrl_key(),
            shift: ev.shift_key(),
            alt: ev.alt_key(),
            in_field: in_field(&ev),
            now_ms: platform::now_ms(),
        };
        let Some((sc, prevent)) = app
            .keys
            .try_update_value(|st| keys::map_key(&ctx, st, &input))
            .flatten()
        else {
            return;
        };
        if prevent {
            ev.prevent_default();
        }
        match sc {
            Shortcut::TogglePalette => {
                let open = !app.palette_open.get_untracked();
                ui.palette_query.set(String::new());
                ui.palette_index.set(0);
                app.palette_open.set(open);
            }
            Shortcut::PaletteUp => ui.palette_index.update(|i| *i = i.saturating_sub(1)),
            Shortcut::PaletteDown => ui.palette_index.update(|i| *i += 1),
            Shortcut::PaletteRun => ui.palette_run.update(|n| *n += 1),
            Shortcut::CloseOverlay => {
                if app.palette_open.get_untracked() {
                    app.palette_open.set(false);
                } else if app.feedback_open.get_untracked() {
                    app.feedback_open.set(false);
                } else if app.modal.get_untracked().is_some() {
                    app.modal.set(None);
                } else {
                    app.lightbox.set(None);
                }
            }
            Shortcut::ClosePeek => app.peek.set(None),
            Shortcut::BackToBoard => app.go(Route::Board),
            Shortcut::Go(s) => app.go(match s {
                Screen::Board | Screen::Task => Route::Board,
                Screen::Planner => Route::Planner(None),
                Screen::Brain => Route::Brain,
                Screen::Settings => Route::Settings(SettingsTab::Credentials),
                Screen::Admin => Route::Admin,
            }),
            Shortcut::NewIdea => app.go(Route::NewIdea),
            Shortcut::OpenFull => {
                if let Some(key) = app.peek.get_untracked().and_then(|id| {
                    app.data
                        .with_untracked(|d| d.task(id).map(|t| t.key.clone()))
                }) {
                    app.go(Route::Task(key));
                }
            }
            Shortcut::Task(k) => task_key(app, k),
            Shortcut::PlannerSend => ui.planner_send.update(|n| *n += 1),
            Shortcut::PlannerDispatch => ui.planner_dispatch.update(|n| *n += 1),
            Shortcut::FocusSearch => platform::focus_id("brain-search"),
        }
    });
    on_cleanup(move || handle.remove());

    view! {
        <div class="app">
            {move || match kind.get() {
                AuthKind::Booting => view! { <div class="boot">"connecting to the Mac mini…"</div> }.into_any(),
                AuthKind::Login => view! { <login::Login/> }.into_any(),
                AuthKind::Setup => view! { <login::Setup/> }.into_any(),
                AuthKind::App => view! {
                    <shell::Sidebar/>
                    <main class="main">
                        <shell::Screens/>
                    </main>
                    <palette::Palette/>
                    <modals::Overlays/>
                }.into_any(),
            }}
            <modals::ToastView/>
        </div>
    }
}
