//! Admin: machine capacity, users + limits + spend, running containers, queue.

use super::components::*;
use crate::state::util::*;
use crate::state::*;
use kiln_types::api::UpdateUserRequest;
use kiln_types::*;
use leptos::prelude::*;

#[component]
pub fn Admin() -> impl IntoView {
    let app = use_app();
    let machine = Memo::new(move |_| app.data.with(|d| d.machine.clone()));
    let admin = Memo::new(move |_| app.data.with(|d| d.admin.clone()));
    let is_admin = Memo::new(move |_| app.data.with(|d| d.is_admin()));
    let spec = Memo::new(move |_| {
        machine.with(|m| {
            if m.spec_line.is_empty() {
                m.hostname.clone()
            } else if m.uptime_secs > 0 && !m.spec_line.contains("uptime") {
                format!("{} · uptime {}", m.spec_line, fmt_uptime(m.uptime_secs))
            } else {
                m.spec_line.clone()
            }
        })
    });
    let ctr_gb = Memo::new(move |_| {
        admin.with(|a| {
            a.as_ref()
                .map(|a| a.containers.iter().map(|c| c.mem_mb).sum::<u64>() as f32 / 1024.0)
        })
    });
    view! {
        <header class="topbar" style="gap:10px">
            <span class="topbar-title">"Admin"</span>
            <span class="mono" style="font-size:11px;color:var(--dim)">{move || spec.get()}</span>
            {move || is_admin.get().then(|| view! {
                <button class="btn btn-secondary btn-md ml-auto" on:click=move |_| app.modal.set(Some(Modal::AddProject))>"Add project"</button>
                <button class="btn btn-primary btn-md" on:click=move |_| app.modal.set(Some(Modal::CreateAccount))>"Create account"</button>
            })}
        </header>
        <div class="page">
            <div class="stats">
                <div class="stat">
                    <div class="lb">"CPU"</div>
                    <div class="big">{move || format!("{}%", machine.with(|m| m.cpu_percent.round() as i64))}</div>
                    <div class="cpu-bars">
                        {move || {
                            let hist = machine.with(|m| m.cpu_history.clone());
                            let n = hist.len();
                            hist.into_iter().enumerate().map(|(i, v)| {
                                let c = if i + 1 == n { "var(--running)" } else if v > 85.0 { "var(--human)" } else { "var(--cpu-bar)" };
                                view! { <div style=format!("height:{}%;background:{c}", v.clamp(0.0, 100.0))></div> }
                            }).collect_view()
                        }}
                    </div>
                </div>
                <div class="stat">
                    <div class="lb">"Memory"</div>
                    <div class="big">{move || machine.with(|m| fmt_gb(m.ram_used_gb))}<small>{move || machine.with(|m| format!(" / {} GB", m.ram_total_gb.round() as i64))}</small></div>
                    <div class="mem-bar"><div style=move || format!("width:{}%", machine.with(|m| pct(m.ram_used_gb as f64, m.ram_total_gb as f64)))></div></div>
                    <div class="sub">{move || {
                        let used = machine.with(|m| m.ram_used_gb);
                        match ctr_gb.get() {
                            Some(c) => format!("containers {} GB · system {} GB", fmt_gb(c), fmt_gb((used - c).max(0.0))),
                            None => format!("disk {} / {} GB", machine.with(|m| m.disk_used_gb.round() as i64), machine.with(|m| m.disk_total_gb.round() as i64)),
                        }
                    }}</div>
                </div>
                <div class="stat">
                    <div class="lb">"Containers"</div>
                    <div class="big">{move || machine.with(|m| m.containers_running)}<small>{move || machine.with(|m| format!(" / {} slots", m.container_slots))}</small></div>
                    <div class="pips">
                        {move || {
                            let (n, slots) = machine.with(|m| (m.containers_running, m.container_slots.max(1)));
                            (0..slots).map(|i| view! { <div class=if i < n { "on" } else { "" }></div> }).collect_view()
                        }}
                    </div>
                </div>
                <div class="stat">
                    <div class="lb">"Queue depth"</div>
                    <div class="big">{move || machine.with(|m| m.queue_depth)}</div>
                    <div class="sub">{move || machine.with(|m| format!("est. wait {} · oldest {}", fmt_wait(m.est_wait_secs), fmt_wait(m.oldest_queued_secs).trim_start_matches('~')))}</div>
                </div>
            </div>
            {move || if is_admin.get() {
                view! { <AdminTables/> }.into_any()
            } else {
                view! { <div class="card tbl-empty">"User management, containers and the queue are visible to admins."</div> }.into_any()
            }}
        </div>
    }
}

#[component]
fn AdminTables() -> impl IntoView {
    let app = use_app();
    let users = Memo::new(move |_| {
        app.data.with(|d| {
            d.admin
                .as_ref()
                .map(|a| a.users.clone())
                .unwrap_or_default()
        })
    });
    let containers = Memo::new(move |_| {
        app.data.with(|d| {
            d.admin
                .as_ref()
                .map(|a| a.containers.clone())
                .unwrap_or_default()
        })
    });
    let queue = Memo::new(move |_| {
        app.data.with(|d| {
            d.admin
                .as_ref()
                .map(|a| a.queue.clone())
                .unwrap_or_default()
        })
    });
    view! {
        <div class="admin-grid">
            <div class="list-card">
                <div class="utable-hd"><span>"USER"</span><span>"ROLE"</span><span class="nowrap">"PARALLEL"</span><span class="nowrap">"SPEND / MO"</span></div>
                <For each=move || users.get() key=|r| r.user.id let:row>
                    {
                        let id = row.user.id;
                        let r = Memo::new(move |_| users.with(|u| u.iter().find(|x| x.user.id == id).cloned()));
                        let limit = move || r.with(|r| r.as_ref().map(|r| r.user.limits.concurrency).unwrap_or(1));
                        let set_limit = move |delta: i32| {
                            let Some(mut l) = r.with_untracked(|r| r.as_ref().map(|r| r.user.limits.clone())) else { return };
                            l.concurrency = (l.concurrency as i32 + delta).clamp(1, 8) as u32;
                            app.update_user(id, UpdateUserRequest { limits: Some(l), ..Default::default() });
                        };
                        let role = move || r.with(|r| r.as_ref().map(|r| r.user.role)).unwrap_or(UserRole::Member);
                        let spend = move || r.with(|r| r.as_ref().map(|r| (r.spend.month_usd, r.spend.budget_usd))).unwrap_or((0.0, 1.0));
                        view! {
                            <div class="urow">
                                <div class="who">
                                    {avatar(row.user.initials.clone(), 28, 10.0)}
                                    <div><span class="nm">{row.user.name.clone()}</span><span class="em">{row.user.email.clone()}</span></div>
                                </div>
                                <select class="select" on:change=move |ev| {
                                    let v = if event_target_value(&ev) == "admin" { UserRole::Admin } else { UserRole::Member };
                                    app.update_user(id, UpdateUserRequest { role: Some(v), ..Default::default() });
                                }>
                                    <option value="admin" selected=move || role() == UserRole::Admin>"Admin"</option>
                                    <option value="member" selected=move || role() == UserRole::Member>"Member"</option>
                                </select>
                                <div class="stepper sm">
                                    <span class="b" tabindex="0" on:click=move |_| set_limit(-1)>"−"</span>
                                    <span class="v">{limit}</span>
                                    <span class="b" tabindex="0" on:click=move |_| set_limit(1)>"+"</span>
                                </div>
                                <div class="spend">
                                    <div class="v"><span>{move || format!("${}", spend().0.round() as i64)}</span><span>{move || format!("/ ${}", spend().1.round() as i64)}</span></div>
                                    <div class="bar"><div style=move || {
                                        let (s, b) = spend();
                                        let p = pct(s, b);
                                        format!("width:{p}%;background:var(--{})", if p > 80.0 { "human" } else { "running" })
                                    }></div></div>
                                </div>
                            </div>
                        }
                    }
                </For>
            </div>
            <div style="display:flex;flex-direction:column;gap:20px">
                <div class="list-card">
                    <div class="tbl-hd">"Running containers"<span class="mono">"live"</span></div>
                    <For each=move || containers.get() key=|c| (c.run_id, c.task_key.clone(), c.role, c.cpu_percent.round() as i64) let:c>
                        {
                            let key = c.task_key.clone().unwrap_or_default();
                            let title = app.data.with_untracked(|d| d.task_by_key(&key).map(|t| t.title.clone())).unwrap_or_default();
                            let k2 = key.clone();
                            view! {
                                <div class="ctr-row">
                                    <span class="dot"></span>
                                    <span class="c-muted">{c.role.as_str()}</span>
                                    <span class="task" on:click=move |_| if !k2.is_empty() { app.go(Route::Task(k2.clone())) }>{format!("{key} {title}")}</span>
                                    <span class="c-dim" style="text-align:right">{format!("{:.1}G", c.mem_mb as f64 / 1024.0)}</span>
                                    <span class="c-dim" style="text-align:right">{format!("{}%", c.cpu_percent.round() as i64)}</span>
                                </div>
                            }
                        }
                    </For>
                    {move || containers.with(|c| c.is_empty()).then(|| view! { <div class="tbl-empty">"No containers running"</div> })}
                </div>
                <div class="list-card">
                    <div class="tbl-hd">"Queue"</div>
                    <For each=move || queue.get() key=|q| (q.task_id, q.position) let:q>
                        <div class="q-row">
                            <span class="c-dim">{format!("#{}", q.position)}</span>
                            <span class="ellipsis">{format!("{} {}", q.task_key, q.title)}</span>
                            <span class="c-dim" style="text-align:right">{fmt_wait(q.est_wait_secs)}</span>
                        </div>
                    </For>
                    {move || queue.with(|q| q.is_empty()).then(|| view! { <div class="tbl-empty">"Queue is empty"</div> })}
                </div>
            </div>
        </div>
    }
}
