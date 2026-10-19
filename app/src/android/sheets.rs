//! Bottom sheets: Feedback / Decide, More, Screenshot.

use super::feed;
use super::nav::Sheet;
use super::use_ui;
use kiln_types::api::TaskAction;
use kiln_types::ids::TaskId;
use kiln_types::*;
use leptos::prelude::*;

#[component]
pub fn Sheets() -> impl IntoView {
    let ui = use_ui();
    let sheet = Memo::new(move |_| ui.nav.with(|n| n.sheet.clone()));
    let close = move || ui.nav.update(|n| n.sheet = None);
    view! {
        {move || sheet.get().map(|s| view! {
            <div class="a-scrim" on:click=move |_| close()></div>
            <div class="a-sheet">
                <div class="a-handle"></div>
                {match s {
                    Sheet::Feedback(t) => view! { <FeedbackSheet task=t/> }.into_any(),
                    Sheet::More(t) => view! { <MoreSheet task=t/> }.into_any(),
                    Sheet::Shot { title, url } => view! {
                        <div class="a-sheet-shot-title">{title}</div>
                        <div class="a-sheet-shot" style=url.map(|u| format!("background-image:url('{u}');background-size:contain;background-repeat:no-repeat;background-position:center")).unwrap_or_default()>
                            "QA capture · pinch to zoom"
                        </div>
                    }.into_any(),
                }}
            </div>
        })}
    }
}

#[component]
fn FeedbackSheet(task: TaskId) -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let t = Memo::new(move |_| app.data.with(|d| d.task(task).cloned()));
    let sugg = Memo::new(move |_| {
        app.data.with(|d| {
            d.task(task)
                .map(|t| feed::suggestions(d, t))
                .unwrap_or_default()
        })
    });
    let text = ui.fb_text;
    if ui.fb_task.get_untracked() != Some(task) {
        ui.fb_task.set(Some(task));
        text.set(String::new());
    }
    let title = move || {
        t.with(|t| match t {
            Some(t) if t.state == TaskState::Escalated => format!("Decide for {}", t.key),
            _ => "Feedback to Builder".into(),
        })
    };
    let sub = move || {
        t.with(|t| {
            t.as_ref()
                .map(|t| {
                    let cap = t.loop_cap.max(1);
                    format!(
                        "{} restarts at attempt {}/{cap} with your note, the QA report and the reviewer verdict.",
                        t.key,
                        (t.attempt() + 1).min(cap)
                    )
                })
                .unwrap_or_default()
        })
    };
    let send = move |_| {
        let body = text.get_untracked();
        if body.trim().is_empty() {
            ui.snack("Write a note or pick a suggestion");
            return;
        }
        let card = ui.task_card(task);
        app.task_action(
            task,
            TaskAction::Feedback {
                text: body.trim().to_string(),
                attach_qa: true,
                attach_verdict: true,
                save_memory: false,
            },
        );
        if let Some(c) = card {
            ui.resolve(&c, "You replied · Builder restarted".into(), None);
        }
        text.set(String::new());
        ui.nav.update(|n| n.sheet = None);
    };
    view! {
        <div class="a-sheet-title">{title}</div>
        <div class="a-sheet-sub">{sub}</div>
        <div class="a-quick">
            {move || sugg.get().into_iter().map(|q| {
                let q2 = q.clone();
                view! { <span class="a-quick-chip" on:click=move |_| text.set(q2.clone())>{q}</span> }
            }).collect_view()}
        </div>
        <textarea class="a-filled" rows="3" placeholder="Or write your own…"
            prop:value=move || text.get()
            on:input=move |ev| text.set(event_target_value(&ev))></textarea>
        <button class="a-btn primary wide tall" on:click=send>"Send & restart Builder"</button>
    }
}

#[component]
fn MoreSheet(task: TaskId) -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let key = Memo::new(move |_| {
        app.data
            .with(|d| d.task(task).map(|t| t.key.clone()).unwrap_or_default())
    });
    let close = move || ui.nav.update(|n| n.sheet = None);
    let acts: [(&str, &str, &str, &str, u8); 3] = [
        (
            "■",
            "#f0605a",
            "Stop task",
            "Kills the container and keeps the artifacts",
            0,
        ),
        (
            "↗",
            "#c4c7b6",
            "Open PR on GitHub",
            "Opens in the browser",
            1,
        ),
        (
            "≡",
            "#c4c7b6",
            "View logs",
            "Opens the TUI log stream on desktop",
            2,
        ),
    ];
    view! {
        <div class="a-sheet-id">{key}</div>
        {acts.into_iter().map(|(g, c, l, d, i)| view! {
            <div class="a-more-row" on:click=move |_| {
                close();
                match i {
                    0 => app.task_action(task, TaskAction::Stop),
                    1 => app.open_pr(task),
                    _ => ui.snack("Sent to your desktop"),
                }
            }>
                <span class="g" style=format!("color:{c}")>{g}</span>
                <div class="tx"><span class="l">{l}</span><span class="d">{d}</span></div>
            </div>
        }).collect_view()}
    }
}
