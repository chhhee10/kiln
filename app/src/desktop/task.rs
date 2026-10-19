//! Task detail full page.

use super::components::*;
use super::task_key;
use crate::state::derive;
use crate::state::keys::TaskKey;
use crate::state::util::*;
use crate::state::*;
use kiln_types::*;
use leptos::prelude::*;

fn enumerated<T>(v: Vec<T>) -> Vec<(usize, T)> {
    v.into_iter().enumerate().collect()
}

#[component]
pub fn TaskPage() -> impl IntoView {
    let app = use_app();
    let key = Memo::new(move |_| match app.route.get() {
        Route::Task(k) => k,
        _ => String::new(),
    });
    let task = Memo::new(move |_| {
        let k = key.get();
        app.data.with(|d| d.task_by_key(&k).cloned())
    });
    // Select the task once it's known (deep link before the board loaded).
    Effect::new(move |prev: Option<Option<TaskId>>| {
        let id = task.with(|t| t.as_ref().map(|t| t.id));
        if id.is_some()
            && prev.flatten() != id
            && app.selected_task.get_untracked() != id
            && let Some(id) = id
        {
            app.select_task(id);
        }
        id
    });
    let detail = Memo::new(move |_| {
        task.with(|t| t.as_ref().map(|t| t.id))
            .and_then(|id| app.data.with(|d| d.details.get(&id).cloned()))
    });
    let stages = Memo::new(move |_| {
        app.tick.track();
        task.get()
            .map(|t| app.data.with(|d| derive::stages(d, &t, chrono::Utc::now())))
            .unwrap_or_default()
    });
    let status = move || task.with(|t| t.as_ref().map(|t| t.status()).unwrap_or(Status::Queued));
    let sup = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref().map(|t| {
                app.data.with(|d| {
                    let p = d
                        .project(t.project_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    let plan = d
                        .plans
                        .get(&t.plan_id)
                        .map(|p| p.spec.feature.clone())
                        .unwrap_or_default();
                    if plan.is_empty() {
                        p
                    } else {
                        format!("{p} · plan “{plan}”")
                    }
                })
            })
        })
        .unwrap_or_default()
    });
    let started = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref()
                .and_then(|d| d.runs.iter().filter_map(|r| r.started_at).min())
                .map(fmt_hhmm)
                .unwrap_or_else(|| "not yet".into())
        })
    });
    let criteria = Memo::new(move |_| {
        let from_detail = detail
            .with(|d| d.as_ref().map(|d| d.criteria.clone()))
            .unwrap_or_default();
        if !from_detail.is_empty() {
            return from_detail;
        }
        task.with(|t| {
            t.as_ref()
                .map(|t| {
                    t.acceptance
                        .iter()
                        .map(|c| CriterionStatus {
                            criterion: c.clone(),
                            status: Status::Queued,
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    });

    view! {
        <header class="topbar" style="gap:10px;font-size:13px">
            <span class="crumb" on:click=move |_| app.go(Route::Board)>"Board"</span>
            <span style="color:var(--faint)">"/"</span>
            <span class="mono" style="font-size:12px">{move || key.get()}</span>
            <span class="kbd" style="margin-left:4px;padding:1px 6px">"esc"</span>
            <a class="mono" style="margin-left:auto;font-size:12px;display:flex;align-items:center;gap:6px"
                on:click=move |ev| { ev.prevent_default(); if let Some(t) = task.get_untracked() { app.open_pr(t.id) } }>
                {move || task.with(|t| t.as_ref().map(pr_label).unwrap_or_default())}
            </a>
        </header>
        {move || if task.with(|t| t.is_none()) {
            view! { <div class="page"><div class="none-yet">{move || format!("No task {} on the board.", key.get())}</div></div> }.into_any()
        } else {
            view! {
                <div class="page">
                    <div class="hero">
                        <div class="hero-l">
                            <div class="hero-sup">{move || sup.get()}</div>
                            <div class="hero-title">{move || task.with(|t| t.as_ref().map(|t| t.title.clone()).unwrap_or_default())}</div>
                            <div class="meta-row" style="flex-wrap:nowrap">
                                {status_pill(status)}
                                <span>"Attempt "<span class="att">{move || task.with(|t| t.as_ref().map(|t| format!("{}/{}", t.attempt().min(t.loop_cap), t.loop_cap)).unwrap_or_default())}</span></span>
                                <span>"·"</span>
                                <span>{move || task.with(|t| t.as_ref().map(|t| format!("Owner {}", app.data.with(|d| d.user_name(t.owner_id))))).unwrap_or_default()}</span>
                                <span>"·"</span>
                                <span>{move || format!("Started {}", started.get())}</span>
                            </div>
                        </div>
                        <div style="display:flex;gap:8px">
                            <button class="btn btn-primary btn-lg" on:click=move |_| task_key(app, TaskKey::Feedback)>"Send feedback to Builder"{k("F")}</button>
                            <button class="btn btn-secondary btn-lg" on:click=move |_| task_key(app, TaskKey::Retry)>"Retry"{k("R")}</button>
                            <button class="btn btn-secondary btn-lg" on:click=move |_| task_key(app, TaskKey::Stop)>"Stop"{k("S")}</button>
                            <button class="btn btn-secondary btn-lg btn-warn" on:click=move |_| task_key(app, TaskKey::Escalate)>"Escalate"{k("E")}</button>
                        </div>
                    </div>
                    <div class="task-grid">
                        <div class="task-col">
                            {timeline(stages, true)}
                            {log_pane(task, true)}
                            <div class="box big">
                                <div class="section-label">"Acceptance criteria"</div>
                                <For each=move || enumerated(criteria.get()) key=|(i, c)| (*i, c.criterion.clone(), c.status) let:c>
                                    <div class="ac-row">
                                        <span class="g" style=format!("color:var(--{})", var(c.1.status))>{c.1.status.glyph()}</span>
                                        <span class="t">{c.1.criterion.clone()}</span>
                                    </div>
                                </For>
                            </div>
                        </div>
                        <div class="task-col">
                            {qa_artifacts(detail, true)}
                            {tests_block(detail, true)}
                            {reviewer_box(detail, true)}
                            {diff_box(detail, true)}
                        </div>
                    </div>
                </div>
            }.into_any()
        }}
    }
}
