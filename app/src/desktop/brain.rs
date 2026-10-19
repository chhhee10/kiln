//! Company brain: search, filters (incl. approval queue), list, detail.

use super::components::*;
use crate::state::derive::{self, BrainFilter};
use crate::state::util::*;
use crate::state::*;
use kiln_types::api::{MemoryDecision, UpdateMemoryRequest};
use kiln_types::memory::*;
use kiln_types::*;
use leptos::prelude::*;

#[component]
pub fn Brain() -> impl IntoView {
    let app = use_app();
    let q = app.brain_query;
    let items = Memo::new(move |_| {
        let f = app.brain_filter.get();
        let query = q.get();
        app.data.with(|d| derive::memory_list(d, f, &query))
    });
    let total = Memo::new(move |_| app.data.with(|d| d.memory_total.max(d.memory.len() as u32)));
    let runs_week = Memo::new(move |_| app.data.with(|d| d.memory_runs_week));
    let selected = Memo::new(move |_| {
        let sel = app.brain_selected.get();
        app.data.with(|d| {
            sel.and_then(|id| d.memory_item(id).cloned())
                .or_else(|| d.memory.first().cloned())
        })
    });
    // Load the detail of the first item on entry.
    Effect::new(move |prev: Option<Option<MemoryId>>| {
        let id = selected.with(|m| m.as_ref().map(|m| m.id));
        if id != prev.flatten()
            && let Some(id) = id
            && !app
                .data
                .with_untracked(|d| d.memory_details.contains_key(&id))
        {
            app.open_memory(id);
        }
        id
    });

    view! {
        <header class="topbar" style="gap:14px">
            <div class="topbar-title">"Company brain"</div>
            <div class="search">
                <span class="slash">"/"</span>
                <input id="brain-search" placeholder="Search memories, conventions, decisions…" prop:value=move || q.get()
                    on:input=move |ev| q.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Escape" { platform::blur_active() }/>
            </div>
            <span class="ml-auto mono" style="font-size:11px;color:var(--dim)">{move || format!("{} memories · used in {} runs this week", total.get(), runs_week.get())}</span>
        </header>
        <div class="brain">
            <div class="side-tabs">
                {BrainFilter::ALL.into_iter().map(|f| {
                    let count = Memo::new(move |_| app.data.with(|d| derive::memory_count(d, f)));
                    let gc = if f == BrainFilter::Queue { "var(--human)" } else { "var(--muted)" };
                    view! {
                        <div class=move || if app.brain_filter.get() == f { "side-tab on" } else { "side-tab" } tabindex="0" on:click=move |_| app.brain_filter.set(f)>
                            <span class="g" style=format!("color:{gc}")>{f.glyph()}</span>{f.label()}<span class="n">{move || count.get()}</span>
                        </div>
                    }
                }).collect_view()}
            </div>
            <div class="mem-list">
                <For each=move || items.get() key=|m| m.id let:m>
                    {mem_row(m.id, items)}
                </For>
                {move || items.with(|i| i.is_empty()).then(|| view! {
                    <div class="mem-empty">{move || if q.get().trim().is_empty() { "Nothing in this view yet.".to_string() } else { format!("No memories match “{}”.", q.get()) }}</div>
                })}
            </div>
            {move || selected.get().map(|m| view! { <Detail id=m.id/> })}
        </div>
    }
}

fn mem_row(id: MemoryId, items: Memo<Vec<MemoryItem>>) -> impl IntoView {
    let app = use_app();
    let m = Memo::new(move |_| items.with(|v| v.iter().find(|m| m.id == id).cloned()));
    let on = move || {
        let sel = app
            .brain_selected
            .get()
            .or_else(|| app.data.with(|d| d.memory.first().map(|m| m.id)));
        sel == Some(id)
    };
    let meta = Memo::new(move |_| {
        app.tick.track();
        m.get().map(|m| {
            app.data.with(|d| {
                (
                    m.visibility.glyph(),
                    derive::visibility_label(d, &m),
                    m.source_label.clone().unwrap_or_default(),
                    short_person(&d.user_name(m.owner_id)),
                    format!(
                        "used {}× · {}",
                        m.use_count,
                        fmt_ago(m.updated_at, chrono::Utc::now())
                    ),
                )
            })
        })
    });
    let show_approve = move || app.brain_filter.get() == BrainFilter::Queue;
    view! {
        <div class=move || if on() { "mem-row on" } else { "mem-row" } tabindex="0" on:click=move |_| app.open_memory(id)>
            <div class="t">
                <b>{move || m.with(|m| m.as_ref().map(|m| m.title.clone()).unwrap_or_default())}</b>
                {move || m.with(|m| m.as_ref().is_some_and(|m| m.status == MemoryStatus::Pending)).then(|| view! {
                    <span class="pending-tag"><span class="dot"></span>"Pending approval"</span>
                })}
            </div>
            <div class="body">{move || m.with(|m| m.as_ref().map(|m| m.body.clone()).unwrap_or_default())}</div>
            <div class="m">
                {move || meta.get().map(|(g, vl, src, author, used)| view! {
                    <span class="vis"><span class="glyph">{g}</span>{vl}</span>
                    <span>{src}</span>
                    <span>{author}</span>
                    <span class="used">{used}</span>
                })}
            </div>
            {move || show_approve().then(|| view! {
                <div class="acts">
                    <button class="btn btn-primary btn-sm" on:click=move |ev| { ev.stop_propagation(); app.decide_memory(id, MemoryDecision::Approve); }>"Approve to Company"</button>
                    <button class="btn btn-secondary btn-sm" on:click=move |ev| { ev.stop_propagation(); app.decide_memory(id, MemoryDecision::KeepShared); }>"Keep shared"</button>
                </div>
            })}
        </div>
    }
}

#[component]
fn Detail(id: MemoryId) -> impl IntoView {
    let app = use_app();
    let m = Memo::new(move |_| app.data.with(|d| d.memory_item(id).cloned()));
    let detail = Memo::new(move |_| app.data.with(|d| d.memory_details.get(&id).cloned()));
    let people = Memo::new(move |_| app.data.with(|d| d.users.clone()));
    let vis = move || {
        m.with(|m| m.as_ref().map(|m| m.visibility))
            .unwrap_or(Visibility::Private)
    };
    let locked = move || m.with(|m| m.as_ref().is_some_and(|m| m.kind == MemoryType::Preference));
    let header = Memo::new(move |_| {
        app.tick.track();
        m.get().map(|m| {
            app.data.with(|d| {
                format!(
                    "{} · by {} · updated {}",
                    m.source_label.clone().unwrap_or_else(|| "manual".into()),
                    short_person(&d.user_name(m.owner_id)),
                    fmt_ago(m.updated_at, chrono::Utc::now())
                )
            })
        })
    });
    let tags = Memo::new(move |_| {
        m.get()
            .map(|m| app.data.with(|d| derive::memory_tags(d, &m)))
            .unwrap_or_default()
    });
    let set_vis = move |v: Visibility| {
        if locked() {
            app.toast("Personal preferences are always Private", false);
            return;
        }
        let cur = vis();
        if v == cur
            && !m.with_untracked(|m| {
                m.as_ref()
                    .is_some_and(|m| m.status == MemoryStatus::Pending)
            })
        {
            return;
        }
        let mut req = UpdateMemoryRequest {
            visibility: Some(v),
            ..Default::default()
        };
        if v == Visibility::Shared
            && m.with_untracked(|m| m.as_ref().is_some_and(|m| m.shared_with.is_empty()))
        {
            req.shared_with = app
                .data
                .with_untracked(|d| d.me.as_ref().map(|u| vec![u.id]));
        }
        app.update_memory(id, req);
    };
    let toggle_person = move |u: UserId| {
        let Some(mut list) = m.with_untracked(|m| m.as_ref().map(|m| m.shared_with.clone())) else {
            return;
        };
        if let Some(i) = list.iter().position(|x| *x == u) {
            list.remove(i);
        } else {
            list.push(u);
        }
        // Optimistic: reflect the toggle immediately.
        app.data.update(|d| {
            if let Some(x) = d.memory.iter_mut().find(|x| x.id == id) {
                x.shared_with = list.clone();
            }
        });
        app.update_memory(
            id,
            UpdateMemoryRequest {
                shared_with: Some(list),
                ..Default::default()
            },
        );
    };
    let seg = move |v: Visibility, label: &'static str| {
        view! {
            <div class=move || if vis() == v { "on" } else { "" } tabindex="0" on:click=move |_| set_vis(v)>
                <span class="g">{v.glyph()}</span>{label}
            </div>
        }
    };
    view! {
        <div class="mem-detail">
            <div class="src">{move || header.get()}</div>
            <div class="ttl">{move || m.with(|m| m.as_ref().map(|m| m.title.clone()).unwrap_or_default())}</div>
            <div class="body">{move || m.with(|m| m.as_ref().map(|m| m.body.clone()).unwrap_or_default())}</div>
            <div class="tags"><For each=move || tags.get() key=|t| t.clone() let:t><span>{format!("#{t}")}</span></For></div>
            <div class="hr"></div>
            <div style="display:flex;flex-direction:column;gap:8px">
                <div class="section-label">"Visibility"</div>
                <div class=move || if locked() { "vis-seg locked" } else { "vis-seg" }>
                    {seg(Visibility::Private, "Private")}
                    {seg(Visibility::Shared, "People")}
                    {seg(Visibility::Company, "Company")}
                </div>
                <div class="help">{move || m.with(|m| m.as_ref().map(derive::visibility_help)).unwrap_or_default()}</div>
                {move || (vis() == Visibility::Shared).then(|| view! {
                    <div class="share-pills">
                        <For each=move || people.get() key=|u| u.id let:u>
                            {
                                let uid = u.id;
                                let on = move || m.with(|m| m.as_ref().is_some_and(|m| m.shared_with.contains(&uid)));
                                view! {
                                    <span class=move || if on() { "share-pill on" } else { "share-pill" } tabindex="0" on:click=move |_| toggle_person(uid)>
                                        {avatar(u.initials.clone(), 20, 9.0)}{first_name(&u.name)}
                                    </span>
                                }
                            }
                        </For>
                    </div>
                })}
                {move || m.with(|m| m.as_ref().is_some_and(|m| m.status == MemoryStatus::Pending)).then(|| view! {
                    <div style="display:flex;gap:8px">
                        <button class="btn btn-primary btn-sm" on:click=move |_| app.decide_memory(id, MemoryDecision::Approve)>"Approve to Company"</button>
                        <button class="btn btn-secondary btn-sm" on:click=move |_| app.decide_memory(id, MemoryDecision::KeepShared)>"Keep shared"</button>
                    </div>
                })}
            </div>
            <div style="display:flex;flex-direction:column;gap:6px">
                <div class="section-label">"Recently used by"</div>
                <div class="used-by">
                    {move || {
                        app.tick.track();
                        let now = chrono::Utc::now();
                        let uses = detail.with(|d| d.as_ref().map(|d| d.recently_used.clone())).unwrap_or_default();
                        if uses.is_empty() {
                            return view! { <span class="c-faint">"Not used by any run yet"</span> }.into_any();
                        }
                        uses.into_iter().map(|u| {
                            let line = if u.role == WorkerRole::Planner {
                                format!("Planner · {} · {}", u.task_key, fmt_ago(u.at, now))
                            } else {
                                format!("{} · {} · {}", u.task_key, u.role.label(), fmt_ago(u.at, now))
                            };
                            view! { <div>{line}</div> }
                        }).collect_view().into_any()
                    }}
                </div>
            </div>
            <div style="margin-top:auto;display:flex;gap:8px">
                <button class="btn btn-secondary btn-sm" on:click=move |_| app.update_memory(id, UpdateMemoryRequest { archived: Some(true), ..Default::default() })>"Archive"</button>
            </div>
        </div>
    }
}
