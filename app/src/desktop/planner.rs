//! Planner: threads | chat | editable plan card.

use super::components::*;
use super::use_ui;
use crate::state::util::*;
use crate::state::*;
use kiln_types::ids::*;
use kiln_types::plan::*;
use kiln_types::*;
use leptos::html;
use leptos::prelude::*;

const GREETING: &str =
    "What should we build? Describe the idea and I'll turn it into tasks with acceptance criteria.";

fn thread_status(s: PlanStatus) -> (&'static str, &'static str) {
    match s {
        PlanStatus::Drafting => ("Drafting", "human"),
        PlanStatus::Dispatched => ("Dispatched", "running"),
        PlanStatus::Queued => ("Queued", "queued"),
        PlanStatus::Merged => ("Merged", "passed"),
        PlanStatus::Archived => ("Archived", "faint"),
    }
}

#[component]
pub fn Planner() -> impl IntoView {
    let app = use_app();
    let ui = use_ui();
    // Which thread is open (None = a new, not yet created idea).
    let thread_id = Memo::new(move |_| match app.route.get() {
        Route::Planner(Some(t)) => Some(t),
        Route::Planner(None) => app.data.with(|d| d.threads.first().map(|t| t.id)),
        _ => None,
    });
    let threads = Memo::new(move |_| app.data.with(|d| d.threads.clone()));
    let detail = Memo::new(move |_| {
        thread_id
            .get()
            .and_then(|t| app.data.with(|d| d.thread_details.get(&t).cloned()))
    });
    let plan = Memo::new(move |_| {
        thread_id
            .get()
            .and_then(|t| app.data.with(|d| d.plan_for_thread(t).cloned()))
    });
    let pending = Memo::new(move |_| {
        thread_id
            .get()
            .and_then(|t| app.data.with(|d| d.planner_status.get(&t).cloned()))
    });
    let project_id = Memo::new(move |_| {
        thread_id
            .get()
            .and_then(|t| app.data.with(|d| d.thread(t).map(|t| t.project_id)))
            .or_else(|| app.new_idea_project.get())
            .or_else(|| app.board_filter.with(|f| f.project))
            .or_else(|| app.data.with(|d| d.projects.first().map(|p| p.id)))
    });
    let project_short = Memo::new(move |_| {
        project_id
            .get()
            .and_then(|p| {
                app.data
                    .with(|d| d.project(p).map(crate::state::util::project_short))
            })
            .unwrap_or_default()
    });

    // ---- composer
    let draft = RwSignal::new(String::new());
    let send = move || {
        let body = draft.get_untracked();
        if body.trim().is_empty() || pending.get_untracked().is_some() {
            return;
        }
        let Some(project) = project_id.get_untracked() else {
            app.toast("Register a project first (Admin)", false);
            return;
        };
        draft.set(String::new());
        app.send_message(thread_id.get_untracked(), project, body);
    };
    Effect::watch(move || ui.planner_send.get(), move |_, _, _| send(), false);

    // ---- messages + autoscroll
    let msgs = Memo::new(move |_| match detail.get() {
        Some(d) => d.messages,
        None => vec![Message {
            id: MessageId::nil(),
            thread_id: ThreadId::nil(),
            author: MessageAuthor::Planner,
            body: GREETING.into(),
            voice: false,
            plan_version: None,
            created_at: chrono::Utc::now(),
        }],
    });
    let scroller = NodeRef::<html::Div>::new();
    Effect::new(move |_| {
        msgs.with(|m| m.len());
        pending.track();
        if let Some(el) = scroller.get() {
            request_animation_frame(move || el.set_scroll_top(el.scroll_height()));
        }
    });
    let model = Memo::new(move |_| {
        detail
            .with(|d| d.as_ref().map(|d| d.planner_model.clone()))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "claude-opus-5-5".into())
    });
    let in_context = Memo::new(move |_| {
        detail
            .with(|d| d.as_ref().map(|d| d.memories_in_context))
            .unwrap_or(3)
    });
    let title = Memo::new(move |_| {
        plan.with(|p| p.as_ref().map(|p| p.spec.feature.clone()))
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                thread_id
                    .get()
                    .and_then(|t| app.data.with(|d| d.thread(t).map(|t| t.title.clone())))
            })
            .unwrap_or_else(|| "Untitled idea".into())
    });
    let is_new = move || thread_id.get().is_none();
    let cycle_project = move |_| {
        if thread_id.get_untracked().is_some() {
            return;
        }
        let ids: Vec<ProjectId> = app
            .data
            .with_untracked(|d| d.projects.iter().map(|p| p.id).collect());
        if ids.is_empty() {
            return;
        }
        let cur = project_id.get_untracked();
        let i = cur
            .and_then(|c| ids.iter().position(|x| *x == c))
            .map(|i| (i + 1) % ids.len())
            .unwrap_or(0);
        app.new_idea_project.set(Some(ids[i]));
    };
    let kmod = mod_key();

    view! {
        <div class="planner">
            <div class="threads">
                <div class="threads-hd">"Planner"<span class="new" tabindex="0" on:click=move |_| app.go(Route::NewIdea)>"+ New idea"</span></div>
                <div class="threads-list">
                    <For each=move || threads.get() key=|t| (t.id, format!("{:?}", t.status), t.title.clone()) let:t>
                        {
                            let id = t.id;
                            let (label, color) = thread_status(t.status);
                            let short = app.data.with_untracked(|d| d.project(t.project_id).map(crate::state::util::project_short)).unwrap_or_default();
                            view! {
                                <div class=move || if thread_id.get() == Some(id) { "thread on" } else { "thread" } tabindex="0"
                                    on:click=move |_| app.go(Route::Planner(Some(id)))>
                                    <div class="t">{t.title.clone()}</div>
                                    <div class="m"><span>{short}</span><span style=format!("color:var(--{color})")>{label}</span></div>
                                </div>
                            }
                        }
                    </For>
                </div>
            </div>
            <div class="chat">
                <div class="chat-hd">
                    <span class="ttl">{move || title.get()}</span>
                    <span class=move || if is_new() { "tag click" } else { "tag" } title=move || if is_new() { "Click to change project" } else { "" } on:click=cycle_project>{move || project_short.get()}</span>
                    <span class="chat-model">{move || format!("planner · {}", model.get())}</span>
                </div>
                <div class="msgs" node_ref=scroller>
                    <For each=move || msgs.get() key=|m| m.id let:m>
                        {
                            let you = m.author == MessageAuthor::User;
                            let who = match m.author { MessageAuthor::User => "You", MessageAuthor::Planner => "Planner", MessageAuthor::System => "System" };
                            view! {
                                <div class=if you { "msg you" } else { "msg planner" }>
                                    {(!you).then(|| view! { <div class="p-avatar"><div></div></div> })}
                                    <div class="col2">
                                        <div class="who">{who}</div>
                                        <div class="body"><crate::markdown::Markdown text=m.body.clone()/></div>
                                    </div>
                                </div>
                            }
                        }
                    </For>
                    {move || pending.get().map(|label| view! {
                        <div class="pending-line">
                            <div class="p-avatar pending"><div></div></div>
                            {move || SPINNER[(app.tick.get() % 10) as usize]}" "{label}
                        </div>
                    })}
                </div>
                <div class="composer-wrap">
                    <div class="composer">
                        <textarea rows="3" placeholder="Reply to the planner, or describe a change to the plan…" prop:value=move || draft.get()
                            on:input=move |ev| draft.set(event_target_value(&ev))></textarea>
                        <div class="ft">
                            <span>{move || format!("Uses company brain · {} memories in context", in_context.get())}</span>
                            <button class="btn btn-ink" disabled=move || pending.get().is_some() on:click=move |_| send()>"Send "<span class="k">{format!("{kmod}↵")}</span></button>
                        </div>
                    </div>
                </div>
            </div>
            <PlanCard plan=plan title=title/>
        </div>
    }
}

#[component]
fn PlanCard(plan: Memo<Option<Plan>>, title: Memo<String>) -> impl IntoView {
    let app = use_app();
    let ui = use_ui();
    let local = RwSignal::new(None::<PlanSpec>);
    let dirty = RwSignal::new(false);
    let save_token = StoredValue::new(0u64);
    let open = RwSignal::new(Vec::<String>::new());

    // Adopt the server's plan whenever it changes and we have no unsaved edits.
    Effect::new(move |prev: Option<Option<(PlanId, u32)>>| {
        let cur = plan.with(|p| p.as_ref().map(|p| (p.id, p.version)));
        let changed_plan = prev.flatten().map(|x| x.0) != cur.map(|x| x.0);
        if changed_plan || !dirty.get_untracked() {
            local.set(plan.get_untracked().map(|p| p.spec));
            if changed_plan {
                dirty.set(false);
                open.set(
                    plan.with_untracked(|p| {
                        p.as_ref()
                            .and_then(|p| p.spec.tasks.first().map(|t| t.id.clone()))
                    })
                    .into_iter()
                    .collect(),
                );
            }
        }
        cur
    });
    let editable = Memo::new(move |_| {
        plan.with(|p| p.as_ref().is_some_and(|p| p.status == PlanStatus::Drafting))
    });
    let schedule_save = move || {
        let token = save_token.get_value() + 1;
        save_token.set_value(token);
        set_timeout(
            move || {
                if save_token.get_value() != token || !dirty.get_untracked() {
                    return;
                }
                if let (Some(p), Some(spec)) = (plan.get_untracked(), local.get_untracked()) {
                    app.save_plan(p.id, spec, p.version);
                }
                dirty.set(false);
            },
            std::time::Duration::from_millis(700),
        );
    };
    let edit = move |f: Box<dyn FnOnce(&mut PlanSpec)>| {
        if !editable.get_untracked() {
            return;
        }
        local.update(|s| {
            if let Some(s) = s {
                f(s);
            }
        });
        dirty.set(true);
        schedule_save();
    };
    let dispatch = move || {
        let Some(p) = plan.get_untracked() else {
            app.toast("Nothing to dispatch yet · describe the idea first", false);
            return;
        };
        if p.status != PlanStatus::Drafting {
            app.toast("This plan was already dispatched", false);
            return;
        }
        let spec = local.get_untracked().unwrap_or(p.spec.clone());
        if let Err(e) = spec.validate() {
            app.toast(format!("Can't dispatch: {e}"), false);
            return;
        }
        if dirty.get_untracked() {
            save_token.set_value(save_token.get_value() + 1);
            dirty.set(false);
            app.save_plan(p.id, spec, p.version);
            set_timeout(
                move || app.approve_plan(p.id),
                std::time::Duration::from_millis(if app.is_demo() { 0 } else { 500 }),
            );
        } else {
            app.approve_plan(p.id);
        }
    };
    Effect::watch(
        move || ui.planner_dispatch.get(),
        move |_, _, _| dispatch(),
        false,
    );

    let edited_ago = Memo::new(move |_| {
        app.tick.track();
        plan.with(|p| {
            p.as_ref()
                .map(|p| fmt_ago(p.updated_at, chrono::Utc::now()))
        })
        .unwrap_or_default()
    });
    let status = Memo::new(move |_| plan.with(|p| p.as_ref().map(|p| p.status)));
    let loop_cap = Memo::new(move |_| {
        plan.with(|p| {
            p.as_ref()
                .map(|p| app.data.with(|d| d.loop_cap_for(p.project_id)))
        })
        .unwrap_or(DEFAULT_LOOP_CAP)
    });
    let count = Memo::new(move |_| local.with(|s| s.as_ref().map(|s| s.tasks.len()).unwrap_or(0)));
    let harnesses = Memo::new(move |_| {
        app.data.with(|d| {
            d.harnesses
                .as_ref()
                .map(|c| {
                    c.harnesses
                        .iter()
                        .map(|h| (h.id.clone(), h.label.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let models = Memo::new(move |_| {
        app.data.with(|d| {
            let mut v: Vec<String> = Vec::new();
            for h in d.harnesses.iter().flat_map(|c| c.harnesses.iter()) {
                for m in &h.models {
                    if !v.contains(m) {
                        v.push(m.clone());
                    }
                }
            }
            v
        })
    });
    let task_ids = Memo::new(move |_| {
        local.with(|s| {
            s.as_ref()
                .map(|s| s.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let kmod = mod_key();

    view! {
        <div class="plan">
            <div class="plan-hd">
                <span class="ttl">"Plan"</span>
                <span class="ver">{move || plan.with(|p| p.as_ref().map(|p| format!("draft v{} · edited {}", p.version, edited_ago.get()))).unwrap_or_else(|| "no draft yet".into())}</span>
                {move || match status.get() {
                    Some(PlanStatus::Drafting) | None => view! { <span class="st c-human"><span class="dot" style="background:var(--human)"></span>"Needs approval"</span> }.into_any(),
                    Some(s) => {
                        let (l, c) = thread_status(s);
                        view! { <span class="st" style=format!("color:var(--{c})")><span class="dot" style=format!("background:var(--{c})")></span>{l}</span> }.into_any()
                    }
                }}
            </div>
            <div class="plan-body">
                <input class="plan-title" disabled=move || !editable.get()
                    prop:value=move || local.with(|s| s.as_ref().map(|s| s.feature.clone())).unwrap_or_else(|| title.get())
                    on:input=move |ev| { let v = event_target_value(&ev); edit(Box::new(move |s| s.feature = v)); }/>
                {move || {
                    let desc = local.with(|s| s.as_ref().map(|s| s.description.clone())).unwrap_or_default();
                    if plan.with(|p| p.is_none()) {
                        Some(view! { <div class="plan-empty">"The plan appears here once the planner drafts it: tasks, acceptance criteria and a harness + model per worker."</div> }.into_any())
                    } else if desc.is_empty() { None } else {
                        Some(view! { <div class="plan-desc">{desc}</div> }.into_any())
                    }
                }}
                <For each=move || task_ids.get() key=|id| id.clone() let:tid>
                    {plan_task(tid, local, open, editable, edit, harnesses, models)}
                </For>
                {move || editable.get().then(|| view! {
                    <span class="add-task" tabindex="0" on:click=move |_| {
                        let defaults = app.data.with_untracked(|d| d.settings.as_ref().map(|s| s.worker_defaults.clone())).unwrap_or_default();
                        let n = local.with_untracked(|s| s.as_ref().map(|s| s.tasks.len()).unwrap_or(0));
                        let id = format!("t{}", n + 1);
                        open.update(|o| o.push(id.clone()));
                        edit(Box::new(move |s| s.tasks.push(TaskSpec {
                            id,
                            title: "New task".into(),
                            description: String::new(),
                            acceptance_criteria: vec![String::new()],
                            files_hint: vec![],
                            depends_on: vec![],
                            workers: PlanWorkers {
                                builder: defaults.get(&WorkerRole::Builder).cloned(),
                                qa: defaults.get(&WorkerRole::Qa).cloned(),
                                reviewer: defaults.get(&WorkerRole::Reviewer).cloned(),
                            },
                        })));
                    }>"+ Add task"</span>
                })}
            </div>
            <div class="plan-ft">
                <div class="sum">
                    <span>{move || format!("{} task{} · up to {} worker runs", count.get(), if count.get() == 1 { "" } else { "s" }, count.get() as u32 * 3 * loop_cap.get())}</span>
                    <span>{move || format!("loop max {} → escalate", loop_cap.get())}</span>
                </div>
                <button class="btn btn-primary btn-xl" disabled=move || !editable.get() on:click=move |_| dispatch()>
                    "Approve & Dispatch"<span class="k">{format!("{kmod}⇧↵")}</span>
                </button>
            </div>
        </div>
    }
}

fn ac_indices(n: usize) -> Vec<usize> {
    (0..n).collect()
}

#[allow(clippy::too_many_arguments)]
fn plan_task(
    tid: String,
    local: RwSignal<Option<PlanSpec>>,
    open: RwSignal<Vec<String>>,
    editable: Memo<bool>,
    edit: impl Fn(Box<dyn FnOnce(&mut PlanSpec)>) + Copy + Send + Sync + 'static,
    harnesses: Memo<Vec<(String, String)>>,
    models: Memo<Vec<String>>,
) -> impl IntoView {
    let id = StoredValue::new(tid.clone());
    let task = Memo::new(move |_| {
        local.with(|s| {
            s.as_ref()
                .and_then(|s| s.tasks.iter().find(|t| t.id == id.get_value()).cloned())
        })
    });
    let is_open = move || open.with(|o| o.contains(&id.get_value()));
    let with_task = move |f: Box<dyn FnOnce(&mut TaskSpec)>| {
        edit(Box::new(move |s: &mut PlanSpec| {
            if let Some(t) = s.tasks.iter_mut().find(|t| t.id == id.get_value()) {
                f(t);
            }
        }))
    };
    let ac_count = Memo::new(move |_| {
        task.with(|t| t.as_ref().map(|t| t.acceptance_criteria.len()).unwrap_or(0))
    });
    let worker = move |role: WorkerRole| {
        let choice = Memo::new(move |_| {
            task.with(|t| t.as_ref().and_then(|t| t.workers.get(role).cloned()))
        });
        let set = move |f: Box<dyn FnOnce(&mut WorkerChoice)>| {
            with_task(Box::new(move |t: &mut TaskSpec| {
                let slot = match role {
                    WorkerRole::Builder => &mut t.workers.builder,
                    WorkerRole::Qa => &mut t.workers.qa,
                    _ => &mut t.workers.reviewer,
                };
                let mut c = slot
                    .clone()
                    .unwrap_or_else(|| WorkerChoice::new("claude_sdk", "claude-sonnet-5-5"));
                f(&mut c);
                *slot = Some(c);
            }))
        };
        view! {
            <div class="worker-row">
                <span class="role">{role.label()}</span>
                <select class="select" disabled=move || !editable.get()
                    on:change=move |ev| { let v = event_target_value(&ev); set(Box::new(move |c| c.harness = v)); }>
                    {move || {
                        let cur = choice.with(|c| c.as_ref().map(|c| c.harness.clone())).unwrap_or_default();
                        let mut opts = harnesses.get();
                        if !cur.is_empty() && !opts.iter().any(|(id, _)| *id == cur) {
                            opts.push((cur.clone(), cur.clone()));
                        }
                        opts.into_iter().map(|(hid, label)| {
                            let sel = hid == cur;
                            view! { <option value=hid selected=sel>{label}</option> }
                        }).collect_view()
                    }}
                </select>
                <select class="select model" disabled=move || !editable.get()
                    on:change=move |ev| { let v = event_target_value(&ev); set(Box::new(move |c| c.model = v)); }>
                    {move || {
                        let cur = choice.with(|c| c.as_ref().map(|c| c.model.clone())).unwrap_or_default();
                        let mut opts = models.get();
                        if !cur.is_empty() && !opts.contains(&cur) {
                            opts.push(cur.clone());
                        }
                        opts.into_iter().map(|m| {
                            let sel = m == cur;
                            view! { <option value=m.clone() selected=sel>{m.clone()}</option> }
                        }).collect_view()
                    }}
                </select>
            </div>
        }
    };
    view! {
        <div class=move || if is_open() { "ptask open" } else { "ptask" }>
            <div class="ptask-hd">
                <span class="id">{tid.to_uppercase()}</span>
                <input disabled=move || !editable.get() prop:value=move || task.with(|t| t.as_ref().map(|t| t.title.clone()).unwrap_or_default())
                    on:input=move |ev| { let v = event_target_value(&ev); with_task(Box::new(move |t| t.title = v)); }/>
                <span class="chev" tabindex="0" on:click=move |_| open.update(|o| {
                    let id = id.get_value();
                    if let Some(i) = o.iter().position(|x| *x == id) { o.remove(i); } else { o.push(id); }
                })>{move || if is_open() { "▾" } else { "▸" }}</span>
            </div>
            {move || is_open().then(|| view! {
                <div class="ptask-body">
                    <div class="ptask-sec">
                        <div class="caption">"ACCEPTANCE CRITERIA"</div>
                        <For each=move || ac_indices(ac_count.get()) key=|i| *i let:i>
                            <div class="ac-edit">
                                <span class="g">"○"</span>
                                <input disabled=move || !editable.get()
                                    prop:value=move || task.with(|t| t.as_ref().and_then(|t| t.acceptance_criteria.get(i).cloned()).unwrap_or_default())
                                    on:input=move |ev| { let v = event_target_value(&ev); with_task(Box::new(move |t| if let Some(c) = t.acceptance_criteria.get_mut(i) { *c = v })); }/>
                                <span class="x" tabindex="0" on:click=move |_| with_task(Box::new(move |t| if i < t.acceptance_criteria.len() { t.acceptance_criteria.remove(i); }))>"✕"</span>
                            </div>
                        </For>
                        {move || editable.get().then(|| view! {
                            <span class="add-link" tabindex="0" on:click=move |_| with_task(Box::new(|t| t.acceptance_criteria.push(String::new())))>"+ Add criterion"</span>
                        })}
                    </div>
                    <div class="ptask-sec">
                        <div class="caption">"WORKERS · HARNESS + MODEL"</div>
                        {worker(WorkerRole::Builder)}
                        {worker(WorkerRole::Qa)}
                        {worker(WorkerRole::Reviewer)}
                    </div>
                </div>
            })}
        </div>
    }
}
