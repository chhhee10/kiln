//! Plan review: a full-screen dialog with Approve in the top bar,
//! expandable task cards (read-only model chips) and "Ask the planner for
//! changes" (back to chat + voice mode).

use super::feed::{self, CardKey};
use super::use_ui;
use crate::state::util::{first_name, project_short, short_model};
use kiln_types::ids::PlanId;
use kiln_types::*;
use leptos::prelude::*;
use std::collections::BTreeSet;
use std::time::Duration;

#[component]
pub fn PlanReview(plan: PlanId) -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let p = Memo::new(move |_| app.data.with(|d| d.plans.get(&plan).cloned()));
    let header = Memo::new(move |_| {
        app.data.with(|d| {
            d.plans.get(&plan).map(|p| {
                let proj = d
                    .project(p.project_id)
                    .map(project_short)
                    .unwrap_or_default();
                format!(
                    "{proj} · v{} · drafted with {}",
                    p.version,
                    first_name(&d.user_name(p.owner_id))
                )
            })
        })
    });
    let open = RwSignal::new(BTreeSet::from(["t1".to_string()]));
    let close = move || {
        ui.nav.update(|n| {
            n.stack.clear();
            n.tab = super::nav::Tab::Planner;
        })
    };
    let approve = move |_| {
        let item = ui
            .items
            .with_untracked(|v| v.iter().find(|i| i.key == CardKey::Plan(plan)).cloned());
        let drafting =
            p.with_untracked(|p| p.as_ref().is_some_and(|p| p.status == PlanStatus::Drafting));
        close();
        if !drafting {
            ui.snack("This plan was already dispatched");
            return;
        }
        app.approve_plan_in_place(plan, move |tasks| match &item {
            Some(it) => ui.resolve(
                it,
                feed::approved_text(&tasks),
                Some(feed::approved_followup(&tasks)),
            ),
            None => ui.snack(feed::approved_text(&tasks)),
        });
    };
    let ask = move |_| {
        close();
        let nav = ui.nav;
        set_timeout(
            move || nav.update(|n| n.voice = true),
            Duration::from_millis(250),
        );
    };
    view! {
        <div class="a-page a-sheet-in a-dialog">
            <div class="a-appbar">
                <button class="a-icon-btn" aria-label="Close" on:click=move |_| { ui.nav.update(|n| { n.back(); }); }>"✕"</button>
                <span class="a-dialog-title">"Review plan"</span>
                <button class="a-btn primary small right" on:click=approve>"Approve"</button>
            </div>
            <div class="a-scroll a-plan-review">
                {move || p.get().map(|p| view! {
                    <div class="a-pr-head">
                        <span class="m">{header.get().unwrap_or_default()}</span>
                        <span class="t">{p.spec.feature.clone()}</span>
                        {(!p.spec.description.is_empty()).then(|| view! { <span class="d">{p.spec.description.clone()}</span> })}
                    </div>
                    {p.spec.tasks.iter().map(|ts| {
                        let sid = ts.id.clone();
                        let s2 = sid.clone();
                        let is_open = move || open.with(|o| o.contains(&s2));
                        let ac = ts.acceptance_criteria.clone();
                        let chips: Vec<(String, String)> = WorkerRole::PIPELINE.into_iter().map(|r| {
                            let m = ts.workers.get(r).cloned().or_else(|| app.data.with_untracked(|d| d.settings.as_ref().and_then(|s| s.worker_defaults.get(&r).cloned())));
                            (r.label().to_string(), m.map(|m| short_model(&m.model)).unwrap_or_else(|| "default".into()))
                        }).collect();
                        let is_open2 = is_open.clone();
                        view! {
                            <div class="a-ptask">
                                <div class="a-ptask-head" on:click=move |_| open.update(|o| { if !o.remove(&sid) { o.insert(sid.clone()); } })>
                                    <span class="id">{ts.id.to_uppercase()}</span>
                                    <span class="t">{ts.title.clone()}</span>
                                    <span class="chev">{move || if is_open() { "▾" } else { "▸" }}</span>
                                </div>
                                <Show when=is_open2.clone()>
                                    <div class="a-ptask-body">
                                        {ac.iter().map(|a| view! { <div class="ac"><span class="o">"○"</span>{a.clone()}</div> }).collect_view()}
                                        <div class="a-model-chips">
                                            {chips.iter().map(|(r, m)| view! { <span class="a-model-chip"><span class="r">{r.clone()}</span>{m.clone()}</span> }).collect_view()}
                                        </div>
                                    </div>
                                </Show>
                            </div>
                        }
                    }).collect_view()}
                })}
                <button class="a-btn outlined wide tall" on:click=ask>"Ask the planner for changes"</button>
            </div>
        </div>
    }
}
