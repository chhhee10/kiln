//! Planner home: top bar, filter chips, the feed of messages and cards,
//! the composer and the mic FAB.

use super::feed::{self, CardKey, FeedItem, FeedKind, Filter, TaskCardKind};
use super::nav::{Pushed, Sheet, Tab};
use super::{Ui, use_ui};
use kiln_types::api::MemoryDecision;
use kiln_types::api::TaskAction;
use kiln_types::ids::*;
use kiln_types::*;
use leptos::html;
use leptos::prelude::*;

impl Ui {
    /// Send a message to the home conversation (typed or dictated).
    pub fn send(&self, body: String, voice: bool) {
        let body = body.trim().to_string();
        if body.is_empty() {
            return;
        }
        let app = self.app;
        let pick = self.thread.get_untracked();
        let (tid, project) = app.data.with_untracked(|d| {
            let tid = feed::current_thread(d, pick);
            let project = tid
                .and_then(|t| d.thread(t).map(|t| t.project_id))
                .or_else(|| d.projects.first().map(|p| p.id));
            (tid, project)
        });
        let Some(project) = project else {
            self.snack("Register a project first (desktop › Admin)");
            return;
        };
        self.filter.set(Filter::All);
        app.send_message_voice(tid, project, body, voice);
    }

    /// The current, unresolved card for a task, if the feed shows one.
    pub fn task_card(&self, task: TaskId) -> Option<FeedItem> {
        self.items.with_untracked(|v| {
            v.iter()
                .find(|i| matches!(i.key, CardKey::Task(t, _) if t == task) && i.resolved.is_none())
                .cloned()
        })
    }
}

#[component]
pub fn PlannerHome() -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let running = Memo::new(move |_| {
        app.data
            .with(|d| d.tasks.iter().filter(|t| t.state.is_active()).count())
    });
    let host = Memo::new(move |_| {
        app.data.with(|d| {
            let h = d.machine.hostname.clone();
            if h.is_empty() {
                d.server
                    .as_ref()
                    .map(|s| s.hostname.clone())
                    .unwrap_or_else(|| "server".into())
            } else {
                h
            }
        })
    });
    let initials = Memo::new(move |_| {
        app.data.with(|d| {
            d.me.as_ref()
                .map(|u| u.initials.clone())
                .unwrap_or_default()
        })
    });
    let counts = Memo::new(move |_| ui.items.with(|i| feed::counts(i)));
    let visible = Memo::new(move |_| {
        let f = ui.filter.get();
        ui.items.with(|v| {
            v.iter()
                .filter(|i| f.matches(i))
                .map(|i| i.key.clone())
                .collect::<Vec<_>>()
        })
    });
    let thread = Memo::new(move |_| {
        let pick = ui.thread.get();
        app.data.with(|d| feed::current_thread(d, pick))
    });
    let pending = Memo::new(move |_| {
        thread
            .get()
            .and_then(|t| app.data.with(|d| d.planner_status.get(&t).cloned()))
    });

    // ---- scrolling: to the bottom on new items, or to a deep-link target.
    let scroller = NodeRef::<html::Div>::new();
    let scroll_bottom = move || {
        if let Some(el) = scroller.get_untracked() {
            request_animation_frame(move || el.set_scroll_top(el.scroll_height()));
        }
    };
    let scroll_to = move |k: CardKey| {
        let Some(el) = scroller.get_untracked() else {
            return;
        };
        request_animation_frame(move || {
            let target = ui
                .items
                .with_untracked(|v| v.iter().find(|i| k.matches(&i.key)).map(|i| i.key.dom_id()));
            if let Some(id) = target
                && let Some(card) = document().get_element_by_id(&id)
                && let Ok(card) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(card)
            {
                el.set_scroll_top(card.offset_top() - 12);
            }
        });
    };
    Effect::new(move |prev: Option<(usize, bool)>| {
        let n = visible.with(|v| v.len());
        let p = pending.with(|p| p.is_some());
        if let Some(k) = ui.scroll_req.get() {
            ui.scroll_req.set(None);
            scroll_to(k);
        } else if prev.is_none() || prev.is_some_and(|(pn, pp)| pn != n || (p && !pp)) {
            scroll_bottom();
        }
        (n, p)
    });

    let draft = RwSignal::new(String::new());
    let send = move || {
        let body = draft.get_untracked();
        if body.trim().is_empty() {
            return;
        }
        draft.set(String::new());
        ui.send(body, false);
    };

    view! {
        <div class="a-page a-fade">
            <header class="a-topbar">
                <div class="a-brand" on:click=move |_| if app.is_demo() { ui.simulate_push() }><span></span></div>
                <div class="a-topbar-text">
                    <div class="a-topbar-title">"Planner"</div>
                    <div class="a-topbar-sub">
                        <span class="a-pulse"><span></span><span></span></span>
                        {move || format!("{} workers live · {}", running.get(), host.get())}
                    </div>
                </div>
                <button class="a-avatar" on:click=move |_| ui.nav.update(|n| n.tab_to(Tab::You))>{initials}</button>
            </header>
            <div class="a-chips">
                {Filter::ALL.into_iter().map(|f| {
                    let on = move || ui.filter.get() == f;
                    let count = move || counts.with(|c| c.of(f));
                    view! {
                        <button class="a-chip" class:on=on on:click=move |_| ui.filter.set(f)>
                            <span class="chk">{move || if on() { "✓" } else { "" }}</span>
                            {f.label()}
                            <Show when=move || { count() > 0 }>
                                <span class="cnt" class:red=f == Filter::Needs>{count}</span>
                            </Show>
                        </button>
                    }
                }).collect_view()}
            </div>
            <div class="a-feed" node_ref=scroller>
                <For each=move || visible.get() key=|k| k.clone() let:k>
                    <FeedEntry k=k/>
                </For>
                <Show when=move || pending.with(|p| p.is_some())>
                    <div class="a-thinking">
                        <div class="a-mini-brand breathe"><span></span></div>
                        <span>{move || format!("Thinking · {}…", pending.get().unwrap_or_default().trim_end_matches('…'))}</span>
                    </div>
                </Show>
                <Show when=move || visible.with(|v| v.is_empty())>
                    <div class="a-empty">
                        <div class="g">"✓"</div>
                        <div class="t">"All clear"</div>
                        <div class="s">"Nothing in this filter needs you."</div>
                    </div>
                </Show>
            </div>
            <div class="a-composer">
                <div class="a-field">
                    <input
                        placeholder="Ask, approve, or describe an idea"
                        prop:value=move || draft.get()
                        on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { send() }
                    />
                    <Show when=move || !draft.with(|d| d.trim().is_empty())>
                        <button class="a-send" aria-label="Send" on:click=move |_| send()>"↑"</button>
                    </Show>
                </div>
                <button class="a-mic" aria-label="Talk to the planner" on:click=move |_| ui.nav.update(|n| n.voice = true)>
                    <span></span>
                </button>
            </div>
        </div>
    }
}

#[component]
fn FeedEntry(k: CardKey) -> impl IntoView {
    let ui = use_ui();
    let key = k.clone();
    let item = Memo::new(move |_| ui.items.with(|v| v.iter().find(|i| i.key == key).cloned()));
    let key2 = k.clone();
    let hl = move || {
        ui.highlight
            .with(|h| h.as_ref().is_some_and(|h| h.matches(&key2)))
    };
    let body = if k == CardKey::Live {
        view! { <LiveCard hl=Signal::derive(hl)/> }.into_any()
    } else {
        (move || item.get().map(|it| card_view(ui, it, hl()))).into_any()
    };
    view! { <div class="a-entry" id=k.dom_id()>{body}</div> }
}

fn chip(glyph: &'static str, label: String, color: &'static str) -> impl IntoView {
    view! {
        <span class="a-stchip" style=format!("background:{color}24;color:{color}")>
            <span class="g">{glyph}</span>{label}
        </span>
    }
}

fn resolved_line(text: String, plan: bool) -> impl IntoView {
    view! { <div class="a-resolved" class:a-resolved-plan=plan><span class="g">"✓"</span>{text}</div> }
}

fn status_color(s: Status) -> &'static str {
    s.hex()
}

fn card_view(ui: Ui, it: FeedItem, hl: bool) -> AnyView {
    let app = ui.app;
    let resolved = it.resolved.clone();
    let ring = if hl {
        "box-shadow:0 0 0 2px var(--md-primary)"
    } else {
        ""
    };
    match it.kind.clone() {
        FeedKind::Agent { text, time } => view! {
            <div class="a-agent">
                <div class="a-mini-brand"><span></span></div>
                <div class="a-agent-body">
                    <div class="a-agent-text">{text}</div>
                    <div class="a-time">{time}</div>
                </div>
            </div>
        }
        .into_any(),
        FeedKind::User { text, meta } => view! {
            <div class="a-user">
                <div class="a-bubble">{text}</div>
                <div class="a-time">{meta}</div>
            </div>
        }
        .into_any(),
        FeedKind::Live { .. } => ().into_any(),
        FeedKind::TaskCard { task, kind, key, title, label, time, dots, sub } => {
            let failed = kind == TaskCardKind::Failed;
            let (g, c) = if failed { ("✕", status_color(Status::Failed)) } else { ("!", status_color(Status::NeedsHuman)) };
            let k1 = key.clone();
            let item = it.clone();
            let primary = move |_| {
                if failed {
                    let t = app.data.with_untracked(|d| d.task(task).cloned());
                    if let Some(t) = t {
                        app.task_action(task, TaskAction::Retry);
                        ui.resolve(&item, feed::retry_text(&t), Some(feed::retry_followup(&t)));
                    }
                } else {
                    ui.nav.update(|n| n.open_sheet(Sheet::Feedback(task)));
                }
            };
            let secondary = move |_| {
                if failed {
                    ui.nav.update(|n| n.open_sheet(Sheet::Feedback(task)));
                } else {
                    ui.open_task_id(task);
                }
            };
            view! {
                <div class="a-card" style=ring>
                    <div class="a-card-tap" on:click=move |_| ui.open_task(&k1)>
                        <div class="a-card-head">
                            {chip(g, label, c)}
                            <span class="a-id">{key.clone()}</span>
                            <span class="a-card-time">{time}</span>
                        </div>
                        <div class="a-card-title">{title}</div>
                        <div class="a-dots">
                            {dots.into_iter().map(|d| {
                                let col = if failed && d.status == Status::Failed { status_color(Status::Failed) } else { status_color(d.status) };
                                view! {
                                    <span class="a-dot" style=format!("color:{col}")>
                                        <span class="ring" style=format!("border-color:{col}")>{d.glyph}</span>
                                        {d.name}
                                        <span class="conn"></span>
                                    </span>
                                }
                            }).collect_view()}
                        </div>
                        <div class="a-card-sub">{sub}</div>
                    </div>
                    {match resolved {
                        Some(r) => resolved_line(r, false).into_any(),
                        None => view! {
                            <div class="a-actions">
                                <button class="a-btn primary" on:click=primary>{if failed { "Retry" } else { "Decide" }}</button>
                                <button class="a-btn tonal" on:click=secondary>{if failed { "Feedback" } else { "Open" }}</button>
                            </div>
                        }.into_any(),
                    }}
                </div>
            }
            .into_any()
        }
        FeedKind::Prs { rows, time } => view! {
            <div class="a-card a-prs" style=ring>
                <div class="a-card-head mb">
                    {chip("✓", "Ready for review".into(), status_color(Status::Passed))}
                    <span class="a-card-time">{time}</span>
                </div>
                {rows.into_iter().map(|r| {
                    let k = r.key.clone();
                    let meta = format!("PR #{} · ", r.pr.map(|n| n.to_string()).unwrap_or_else(|| "—".into()));
                    view! {
                        <div class="a-pr-row">
                            <div class="a-pr-text">
                                <span class="t">{r.title}</span>
                                <span class="m">
                                    {meta}
                                    {r.adds.map(|a| view! { <span class="add">{format!("+{a}")}</span> " " })}
                                    {r.dels.map(|d| view! { <span class="del">{format!("−{d}")}</span> " · " })}
                                    {r.project}
                                </span>
                            </div>
                            <button class="a-btn outlined" on:click=move |_| ui.open_task(&k)>"Open"</button>
                        </div>
                    }
                }).collect_view()}
            </div>
        }
        .into_any(),
        FeedKind::Memory { id, who, title, body, from, to } => {
            let (i1, i2) = (it.clone(), it.clone());
            let approve = move |_| {
                let text = app.data.with_untracked(|d| d.memory_item(id).map(|m| feed::memory_text(d, m, true)));
                app.decide_memory(id, MemoryDecision::Approve);
                ui.resolve(&i1, text.unwrap_or_else(|| "Approved".into()), None);
            };
            let keep = move |_| {
                let text = app.data.with_untracked(|d| d.memory_item(id).map(|m| feed::memory_text(d, m, false)));
                app.decide_memory(id, MemoryDecision::KeepShared);
                ui.resolve(&i2, text.unwrap_or_else(|| "Kept shared".into()), None);
            };
            view! {
                <div class="a-card a-mem" style=ring>
                    <div class="a-card-head">
                        <span class="a-stchip neutral"><span class="g">"◑"</span>"Company brain"</span>
                        <span class="a-who">{who}</span>
                    </div>
                    <div class="a-mem-title">{format!("“{title}”")}</div>
                    <div class="a-card-sub">{body}" Requested: "{from}" → "<span class="strong">{to}</span>"."</div>
                    {match resolved {
                        Some(r) => resolved_line(r, false).into_any(),
                        None => view! {
                            <div class="a-actions">
                                <button class="a-btn primary" on:click=approve>"Approve"</button>
                                <button class="a-btn tonal" on:click=keep>"Keep shared"</button>
                            </div>
                        }.into_any(),
                    }}
                </div>
            }
            .into_any()
        }
        FeedKind::Plan { id, version, title, rows } => {
            let item = it.clone();
            let approve = move |_| {
                let item = item.clone();
                app.approve_plan_in_place(id, move |tasks| {
                    ui.resolve(&item, feed::approved_text(&tasks), Some(feed::approved_followup(&tasks)));
                });
            };
            view! {
                <div class="a-card a-plan" style=ring>
                    <div class="a-plan-head">
                        <span class="lbl">"PLAN · NEEDS APPROVAL"</span>
                        <span class="v">{format!("v{version}")}</span>
                    </div>
                    <div class="a-plan-title">{title}</div>
                    <div class="a-plan-rows">
                        {rows.into_iter().map(|r| view! {
                            <div class="a-plan-row">
                                <span class="id">{r.id}</span>
                                <div class="tx"><span class="t">{r.title}</span><span class="m">{r.models}</span></div>
                            </div>
                        }).collect_view()}
                    </div>
                    {match resolved {
                        Some(r) => resolved_line(r, true).into_any(),
                        None => view! {
                            <div class="a-actions">
                                <button class="a-btn primary big grow" on:click=approve>"Approve & dispatch"</button>
                                <button class="a-btn pc big" on:click=move |_| ui.nav.update(|n| n.push(Pushed::PlanReview(id)))>"Review"</button>
                            </div>
                        }.into_any(),
                    }}
                </div>
            }
            .into_any()
        }
    }
}

/// LIVE NOW: fine-grained so progress bars animate instead of re-rendering.
#[component]
fn LiveCard(hl: Signal<bool>) -> impl IntoView {
    let ui = use_ui();
    let live = Memo::new(move |_| {
        ui.items.with(|v| {
            v.iter().find_map(|i| match &i.kind {
                FeedKind::Live {
                    used,
                    slots,
                    avg,
                    rows,
                } => Some((*used, *slots, *avg, rows.clone())),
                _ => None,
            })
        })
    });
    let rows = Memo::new(move |_| {
        live.with(|l| {
            l.as_ref()
                .map(|l| l.3.iter().map(|r| r.task).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    view! {
        <div class="a-card a-live" style=move || if hl.get() { "box-shadow:0 0 0 2px var(--md-primary)" } else { "" }
            on:click=move |_| ui.nav.update(|n| n.tab_to(Tab::Runs))>
            <div class="a-live-head">
                <span class="lbl">"LIVE NOW"</span>
                <span class="slots">{move || live.with(|l| l.as_ref().map(|l| format!("{}/{} slots", l.0, l.1)).unwrap_or_default())}</span>
            </div>
            <div class="a-live-big">
                <span class="n">{move || live.with(|l| l.as_ref().map(|l| format!("{}%", l.2)).unwrap_or_default())}</span>
                <span class="s">"average progress"</span>
            </div>
            <div class="a-live-rows">
                <For each=move || rows.get() key=|t| *t let:t>
                    {
                        let row = Memo::new(move |_| live.with(|l| l.as_ref().and_then(|l| l.3.iter().find(|r| r.task == t).cloned())));
                        let salt = row.with_untracked(|r| r.as_ref().map(|r| r.key.len()).unwrap_or(0));
                        view! {
                            <div class="a-live-row">
                                <div class="line">
                                    <span class="spin">{move || feed::spin(ui.app.tick.get(), salt)}</span>
                                    <span class="id">{move || row.with(|r| r.as_ref().map(|r| r.key.clone()).unwrap_or_default())}</span>
                                    <span class="t">{move || row.with(|r| r.as_ref().map(|r| r.title.clone()).unwrap_or_default())}</span>
                                    <span class="role">{move || row.with(|r| r.as_ref().map(|r| r.role).unwrap_or_default())}</span>
                                </div>
                                <div class="a-bar"><div style=move || format!("width:{}%", row.with(|r| r.as_ref().map(|r| r.progress).unwrap_or(0)))></div></div>
                            </div>
                        }
                    }
                </For>
            </div>
        </div>
    }
}
