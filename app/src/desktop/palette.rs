//! ⌘K / Ctrl+K command palette.

use super::use_ui;
use crate::state::derive;
use crate::state::keys::TaskKey;
use crate::state::*;
use kiln_types::ids::TaskId;
use leptos::html;
use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq)]
enum Cmd {
    Go(Route),
    Dispatch,
    Task(TaskKey, TaskId),
    Open(String),
    Notifications,
    SignOut,
    Theme,
}

#[component]
pub fn Palette() -> impl IntoView {
    let app = use_app();
    let ui = use_ui();
    let all = Memo::new(move |_| {
        let mk = crate::state::keys::mod_label(platform::is_mac());
        let mut v: Vec<(String, String, String, Cmd)> = vec![
            (
                "Go to".into(),
                "Board".into(),
                "G B".into(),
                Cmd::Go(Route::Board),
            ),
            (
                "Go to".into(),
                "Planner".into(),
                "G P".into(),
                Cmd::Go(Route::Planner(None)),
            ),
            (
                "Go to".into(),
                "Company brain".into(),
                "G M".into(),
                Cmd::Go(Route::Brain),
            ),
            (
                "Go to".into(),
                "Settings".into(),
                "G S".into(),
                Cmd::Go(Route::Settings(SettingsTab::Credentials)),
            ),
            (
                "Go to".into(),
                "Admin · machine capacity".into(),
                "G A".into(),
                Cmd::Go(Route::Admin),
            ),
            (
                "Create".into(),
                "New idea → Planner".into(),
                "C".into(),
                Cmd::Go(Route::NewIdea),
            ),
            (
                "Plan".into(),
                "Approve & dispatch current plan".into(),
                format!("{mk}⇧↵"),
                Cmd::Dispatch,
            ),
        ];
        app.data.with(|d| {
            let cur = app
                .selected_task
                .get()
                .or_else(|| d.tasks.first().map(|t| t.id));
            if let Some(t) = cur.and_then(|id| d.task(id)) {
                v.push((
                    "Task".into(),
                    format!("Retry {}", t.key),
                    "R".into(),
                    Cmd::Task(TaskKey::Retry, t.id),
                ));
                v.push((
                    "Task".into(),
                    format!("Send feedback to Builder · {}", t.key),
                    "F".into(),
                    Cmd::Task(TaskKey::Feedback, t.id),
                ));
                v.push((
                    "Task".into(),
                    format!("Escalate {}", t.key),
                    "E".into(),
                    Cmd::Task(TaskKey::Escalate, t.id),
                ));
            }
            let unread = d.notifications.iter().filter(|n| !n.read).count();
            if unread > 0 {
                v.push((
                    "Inbox".into(),
                    format!(
                        "{unread} unread notification{} · mark read",
                        if unread == 1 { "" } else { "s" }
                    ),
                    String::new(),
                    Cmd::Notifications,
                ));
            }
            for t in &d.tasks {
                v.push((
                    "Open".into(),
                    format!("{}  {}", t.key, t.title),
                    String::new(),
                    Cmd::Open(t.key.clone()),
                ));
            }
        });
        v.push((
            "View".into(),
            "Toggle light / dark theme".into(),
            String::new(),
            Cmd::Theme,
        ));
        v.push((
            "Account".into(),
            "Sign out".into(),
            String::new(),
            Cmd::SignOut,
        ));
        v
    });
    let filtered = Memo::new(move |_| {
        let q = ui.palette_query.get();
        all.with(|a| {
            let pairs: Vec<(String, String)> = a
                .iter()
                .map(|(g, l, _, _)| (g.clone(), l.clone()))
                .collect();
            derive::palette_filter(&pairs, &q, 9)
                .into_iter()
                .map(|i| a[i].clone())
                .collect::<Vec<_>>()
        })
    });
    // Clamp the selection when the list shrinks.
    Effect::new(move |_| {
        let n = filtered.with(|f| f.len());
        if ui.palette_index.get() >= n.max(1) {
            ui.palette_index.set(n.saturating_sub(1));
        }
    });
    let run = move |c: Cmd| {
        app.palette_open.set(false);
        match c {
            Cmd::Go(r) => app.go(r),
            Cmd::Dispatch => {
                if app.current_screen() != Screen::Planner {
                    app.go(Route::Planner(None));
                }
                ui.planner_dispatch.update(|n| *n += 1);
            }
            Cmd::Task(TaskKey::Feedback, id) => {
                let key = app
                    .data
                    .with_untracked(|d| d.task(id).map(|t| t.key.clone()));
                if let Some(k) = key {
                    app.go(Route::Task(k));
                }
                app.feedback_open.set(true);
            }
            Cmd::Task(k, id) => {
                app.select_task(id);
                super::task_key(app, k);
            }
            Cmd::Open(k) => {
                app.log_tab.set(None);
                app.go(Route::Task(k));
            }
            Cmd::Notifications => app.mark_notifications_read(),
            Cmd::Theme => app.set_theme(if app.theme.get_untracked() == Theme::Dark {
                Theme::Light
            } else {
                Theme::Dark
            }),
            Cmd::SignOut => app.logout(),
        }
    };
    Effect::watch(
        move || ui.palette_run.get(),
        move |_, _, _| {
            let i = ui.palette_index.get_untracked();
            if let Some(c) = filtered.with_untracked(|f| f.get(i).map(|x| x.3.clone())) {
                run(c);
            }
        },
        false,
    );
    let input = NodeRef::<html::Input>::new();
    Effect::new(move |_| {
        if app.palette_open.get()
            && let Some(el) = input.get()
        {
            request_animation_frame(move || {
                let _ = el.focus();
            });
        }
    });
    move || {
        app.palette_open.get().then(|| {
            view! {
                <div class="scrim pal" on:click=move |_| app.palette_open.set(false)>
                    <div class="palette" on:click=|ev| ev.stop_propagation()>
                        <div class="pal-in">
                            <span class="p">"›"</span>
                            <input node_ref=input placeholder="Type a command or search tasks…" prop:value=move || ui.palette_query.get()
                                on:input=move |ev| { ui.palette_query.set(event_target_value(&ev)); ui.palette_index.set(0); }/>
                            <span class="kbd" style="padding:1px 6px">"esc"</span>
                        </div>
                        <div class="pal-list">
                            {move || filtered.get().into_iter().enumerate().map(|(i, (g, l, k, c))| {
                                view! {
                                    <div class=move || if ui.palette_index.get() == i { "pal-row on" } else { "pal-row" }
                                        on:mouseenter=move |_| ui.palette_index.set(i)
                                        on:click=move |_| run(c.clone())>
                                        <span class="g">{g}</span><span class="l">{l}</span><span class="k">{k}</span>
                                    </div>
                                }
                            }).collect_view()}
                            {move || filtered.with(|f| f.is_empty()).then(|| view! { <div class="pal-none">"No results"</div> })}
                        </div>
                        <div class="pal-ft"><span>"↑↓ select"</span><span>"↵ run"</span><span>"g b / g p / g m / g s / g a  jump"</span></div>
                    </div>
                </div>
            }
        })
    }
}
