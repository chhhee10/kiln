//! Board: project/people filters, 7 columns, live cards, side-panel peek.

use super::components::*;
use super::task_key;
use crate::state::derive::{self, CardTarget, CardVm};
use crate::state::keys::TaskKey;
use crate::state::util::*;
use crate::state::*;
use kiln_types::*;
use leptos::prelude::*;

#[component]
pub fn Board() -> impl IntoView {
    let app = use_app();
    let cols = Memo::new(move |_| {
        let f = app.board_filter.get();
        app.data.with(|d| derive::board_columns(d, &f))
    });
    let running = Memo::new(move |_| app.data.with(derive::running_count));
    let projects = Memo::new(move |_| {
        app.data.with(|d| {
            d.projects
                .iter()
                .map(|p| (p.id, project_short(p)))
                .collect::<Vec<_>>()
        })
    });
    let people = Memo::new(move |_| app.data.with(|d| d.users.clone()));
    let has_filter = Memo::new(move |_| app.board_filter.with(|f| f.active()));
    let peek_open = Memo::new(move |_| app.peek.get().is_some());

    let open_card = move |c: &CardVm| match &c.target {
        CardTarget::Task(id) => app.open_peek(*id),
        CardTarget::Plan { thread, owner, .. } => {
            let mine = app
                .data
                .with_untracked(|d| d.me.as_ref().is_some_and(|m| m.id == *owner));
            if mine {
                app.go(Route::Planner(Some(*thread)));
            } else {
                let who = app.data.with_untracked(|d| d.user_name(*owner));
                app.toast(
                    format!("Draft by {who} · only its owner can open the planner thread"),
                    true,
                );
            }
        }
    };

    view! {
        <header class="topbar">
            <div class="topbar-title" style="margin-right:10px">"Board"</div>
            <div class="seg">
                <div class=move || if app.board_filter.with(|f| f.project.is_none()) { "on" } else { "" }
                    on:click=move |_| app.board_filter.update(|f| f.project = None)>"All"</div>
                <For each=move || projects.get() key=|p| p.0 let:p>
                    {
                        let (id, short) = p;
                        view! {
                            <div class=move || if app.board_filter.with(|f| f.project == Some(id)) { "on" } else { "" }
                                on:click=move |_| app.board_filter.update(|f| f.project = Some(id))>{short}</div>
                        }
                    }
                </For>
            </div>
            <div class="people">
                <For each=move || people.get() key=|u| u.id let:u>
                    {
                        let id = u.id;
                        let on = move || app.board_filter.with(|f| f.people.contains(&id));
                        view! {
                            <div title=u.name.clone() tabindex="0"
                                style=move || {
                                    let any = app.board_filter.with(|f| !f.people.is_empty());
                                    format!("box-shadow:0 0 0 1.5px {};opacity:{}", if on() { "var(--accent)" } else { "transparent" }, if !any || on() { "1" } else { ".45" })
                                }
                                on:click=move |_| app.board_filter.update(|f| f.toggle_person(id))>{u.initials.clone()}</div>
                        }
                    }
                </For>
            </div>
            {move || has_filter.get().then(|| view! {
                <div class="clear-filters" on:click=move |_| app.board_filter.set(Default::default())>"Clear filters"</div>
            })}
            <div class="live-ind">
                <span class="dot7"><span style="background:var(--running)"></span><span class="ring" style="background:var(--running)"></span></span>
                {move || format!("{} workers running", running.get())}
            </div>
            <button class="btn btn-primary btn-md" on:click=move |_| app.go(Route::NewIdea)>"New idea"{k("C")}</button>
        </header>
        <div class="board">
            <For each=move || cols.get() key=|c| c.column let:col>
                {
                    let column = col.column;
                    let cards = Memo::new(move |_| cols.with(|cs| cs.iter().find(|c| c.column == column).map(|c| c.cards.clone()).unwrap_or_default()));
                    view! {
                        <div class="col">
                            <div class="col-hd"><span style="font-weight:500">{column.label()}</span><span class="n">{move || cards.with(|c| c.len())}</span></div>
                            <For each=move || cards.get() key=|c| c.dom_key.clone() let:card>
                                {card_view(card.dom_key.clone(), cards, open_card)}
                            </For>
                            {move || cards.with(|c| c.is_empty()).then(|| view! { <div class="empty-col">"Nothing here"</div> })}
                        </div>
                    }
                }
            </For>
            <div style=move || format!("width:{};flex:none", if peek_open.get() { "min(600px,60%)" } else { "0px" })></div>
        </div>
        {move || peek_open.get().then(|| view! { <Peek/> })}
    }
}

/// One card. Reads its own row from the column memo so live updates patch in
/// place (progress width transitions instead of re-mounting).
fn card_view(
    dom_key: String,
    cards: Memo<Vec<CardVm>>,
    open: impl Fn(&CardVm) + Copy + 'static,
) -> impl IntoView {
    let app = use_app();
    let key = dom_key.clone();
    let c = Memo::new(move |_| cards.with(|cs| cs.iter().find(|c| c.dom_key == key).cloned()));
    let status = move || c.with(|c| c.as_ref().map(|c| c.status).unwrap_or(Status::Queued));
    let selected = move || {
        let peek = app.peek.get();
        c.with(|c| c.as_ref().and_then(|c| c.task_id()))
            .is_some_and(|id| peek == Some(id))
    };
    view! {
        <div class=move || if selected() { "bcard sel" } else { "bcard" } tabindex="0"
            on:click=move |_| if let Some(card) = c.get_untracked() { open(&card) }
            on:keydown=move |ev| if ev.key() == "Enter" && let Some(card) = c.get_untracked() { open(&card) }>
            <div class="r1"><span>{move || c.with(|c| c.as_ref().map(|c| c.id_label.clone()).unwrap_or_default())}</span><span>{move || c.with(|c| c.as_ref().map(|c| c.project.clone()).unwrap_or_default())}</span></div>
            <div class="t">{move || c.with(|c| c.as_ref().map(|c| c.title.clone()).unwrap_or_default())}</div>
            <div class="r3">
                {status_dot(status)}
                <span class="meta" style=move || format!("color:var(--{})", var(status()))>{move || c.with(|c| c.as_ref().map(|c| c.meta.clone()).unwrap_or_default())}</span>
                <span class="avatar">{move || c.with(|c| c.as_ref().map(|c| c.owner_initials.clone()).unwrap_or_default())}</span>
            </div>
            {move || (status() == Status::Running).then(|| view! {
                <div class="prog" style=move || format!("width:{}%", c.with(|c| c.as_ref().map(|c| c.progress).unwrap_or(0)))></div>
            })}
        </div>
    }
}

#[component]
fn Peek() -> impl IntoView {
    let app = use_app();
    let task = Memo::new(move |_| {
        app.peek
            .get()
            .and_then(|id| app.data.with(|d| d.task(id).cloned()))
    });
    let detail = Memo::new(move |_| {
        app.peek
            .get()
            .and_then(|id| app.data.with(|d| d.details.get(&id).cloned()))
    });
    let stages = Memo::new(move |_| {
        app.tick.track();
        task.get()
            .map(|t| app.data.with(|d| derive::stages(d, &t, chrono::Utc::now())))
            .unwrap_or_default()
    });
    let status = move || task.with(|t| t.as_ref().map(|t| t.status()).unwrap_or(Status::Queued));
    let project = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref().and_then(|t| {
                app.data
                    .with(|d| d.project(t.project_id).map(project_short))
            })
        })
        .unwrap_or_default()
    });
    let open_full = move || {
        if let Some(t) = task.get_untracked() {
            app.go(Route::Task(t.key));
        }
    };
    view! {
        <div class="peek">
            <div class="peek-hd">
                <div class="peek-crumb">
                    <span>{move || project.get()}</span><span>"/"</span>
                    <span style="color:var(--muted)">{move || task.with(|t| t.as_ref().map(|t| t.key.clone()).unwrap_or_default())}</span>
                    <span class="ml-auto" style="display:flex;gap:6px">
                        <span class="chip-btn" tabindex="0" on:click=move |_| open_full()>"Open full  O"</span>
                        <span class="chip-btn" tabindex="0" on:click=move |_| app.peek.set(None)>"esc ✕"</span>
                    </span>
                </div>
                <div class="peek-title">{move || task.with(|t| t.as_ref().map(|t| t.title.clone()).unwrap_or_default())}</div>
                <div class="meta-row">
                    {status_pill(status)}
                    <span>"Attempt "<span class="att">{move || task.with(|t| t.as_ref().map(|t| format!("{}/{}", t.attempt().min(t.loop_cap), t.loop_cap)).unwrap_or_default())}</span></span>
                    <span>"·"</span>
                    <span>{move || task.with(|t| t.as_ref().map(|t| app.data.with(|d| d.user_name(t.owner_id))).unwrap_or_default())}</span>
                    <span>"·"</span>
                    <a class="mono" on:click=move |ev| { ev.prevent_default(); if let Some(t) = task.get_untracked() { app.open_pr(t.id) } }>
                        {move || task.with(|t| t.as_ref().map(pr_label).unwrap_or_default())}
                    </a>
                </div>
                <div class="actions">
                    <button class="btn btn-primary" on:click=move |_| task_key(app, TaskKey::Feedback)>"Feedback to Builder"{k("F")}</button>
                    <button class="btn btn-secondary" on:click=move |_| task_key(app, TaskKey::Retry)>"Retry"{k("R")}</button>
                    <button class="btn btn-secondary" on:click=move |_| task_key(app, TaskKey::Stop)>"Stop"{k("S")}</button>
                    <button class="btn btn-secondary btn-warn push" on:click=move |_| task_key(app, TaskKey::Escalate)>"Escalate"{k("E")}</button>
                </div>
            </div>
            <div class="peek-body">
                {timeline(stages, false)}
                {log_pane(task, false)}
                <div class="grid-2 qa">
                    {qa_artifacts(detail, false)}
                    {tests_block(detail, false)}
                </div>
                <div class="grid-2">
                    {reviewer_box(detail, false)}
                    {diff_box(detail, false)}
                </div>
            </div>
        </div>
    }
}
