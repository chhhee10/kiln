//! App shell: 220px sidebar (brand, search, nav, projects, machine card,
//! user row) and the screen switch.

use super::components::*;
use super::use_ui;
use crate::state::derive;
use crate::state::util::*;
use crate::state::*;
use leptos::prelude::*;

#[component]
pub fn Sidebar() -> impl IntoView {
    let app = use_app();
    let ui = use_ui();
    let screen = Memo::new(move |_| app.route.with(|r| r.screen()));
    let nav = [
        (Screen::Board, "Board", "G B", Route::Board),
        (Screen::Planner, "Planner", "G P", Route::Planner(None)),
        (Screen::Brain, "Company brain", "G M", Route::Brain),
        (
            Screen::Settings,
            "Settings",
            "G S",
            Route::Settings(SettingsTab::Credentials),
        ),
        (Screen::Admin, "Admin", "G A", Route::Admin),
    ];
    let projects = Memo::new(move |_| {
        app.data.with(|d| {
            d.projects
                .iter()
                .map(|p| (p.id, p.name.clone(), derive::open_count(d, p.id)))
                .collect::<Vec<_>>()
        })
    });
    let machine = Memo::new(move |_| app.data.with(|d| d.machine.clone()));
    let me = Memo::new(move |_| app.data.with(|d| d.me.clone()));
    let mk_kmod = mod_key();
    view! {
        <aside class="sidebar">
            <div class="sb-brand"><div class="brand-mark"></div>{kiln_types::PRODUCT_NAME}</div>
            <div class="sb-search" tabindex="0" on:click=move |_| { ui.palette_query.set(String::new()); ui.palette_index.set(0); app.palette_open.set(true); }>
                "Search or run…"<span class="kbd">{format!("{}K", mk_kmod)}</span>
            </div>
            <nav class="sb-nav">
                {nav.into_iter().map(|(s, label, keys, route)| {
                    let on = move || screen.get() == s || (s == Screen::Board && screen.get() == Screen::Task);
                    view! {
                        <div class=move || if on() { "sb-item on" } else { "sb-item" } tabindex="0" on:click=move |_| app.go(route.clone())>
                            <span class="sq"></span>{label}<span class="k">{keys}</span>
                        </div>
                    }
                }).collect_view()}
            </nav>
            <div class="sb-projects">
                <div class="caption">"PROJECTS"</div>
                <For each=move || projects.get() key=|p| (p.0, p.2) let:p>
                    {
                        let (id, name, count) = p;
                        let on = move || screen.get() == Screen::Board && app.board_filter.with(|f| f.project == Some(id));
                        view! {
                            <div class=move || if on() { "sb-proj on" } else { "sb-proj" } on:click=move |_| {
                                app.board_filter.update(|f| f.project = Some(id));
                                app.go(Route::Board);
                            }>{name}<span class="n">{count}</span></div>
                        }
                    }
                </For>
            </div>
            <div class="machine" tabindex="0" on:click=move |_| app.go(Route::Admin)>
                <div class="hd">
                    <span class="dot" style=move || format!("background:var(--{})", if machine.with(|m| m.runtime_ok || m.hostname.is_empty()) { "passed" } else { "failed" })></span>
                    {move || machine.with(|m| if m.hostname.is_empty() { "mac-mini".to_string() } else { m.hostname.clone() })}
                    <span class="mono">"tailnet"</span>
                </div>
                <div class="metric">
                    <div class="kv">"CPU"<span class="mono">{move || format!("{}%", machine.with(|m| m.cpu_percent.round() as i64))}</span></div>
                    <div class="bar"><div style=move || format!("width:{}%", machine.with(|m| m.cpu_percent.clamp(0.0, 100.0)))></div></div>
                </div>
                <div class="metric">
                    <div class="kv">"RAM"<span class="mono">{move || machine.with(|m| format!("{} / {} GB", fmt_gb(m.ram_used_gb), m.ram_total_gb.round() as i64))}</span></div>
                    <div class="bar"><div style=move || format!("width:{}%", machine.with(|m| pct(m.ram_used_gb as f64, m.ram_total_gb as f64)))></div></div>
                </div>
                <div class="kv">"Containers"<span class="mono">{move || machine.with(|m| format!("{} / {}", m.containers_running, m.container_slots.max(1)))}</span></div>
                <div class="kv">"Queue"<span class="mono">{move || machine.with(|m| m.queue_depth)}</span></div>
            </div>
            <div class="sb-user">
                {move || me.get().map(|u| avatar(u.initials.clone(), 24, 10.0))}
                <span class="ellipsis">{move || me.with(|u| u.as_ref().map(|u| u.name.clone()).unwrap_or_default())}</span>
                <span class="out" tabindex="0" on:click=move |_| app.logout()>"Sign out"</span>
            </div>
        </aside>
    }
}

#[component]
pub fn Screens() -> impl IntoView {
    let app = use_app();
    let screen = Memo::new(move |_| app.route.with(|r| r.screen()));
    move || match screen.get() {
        Screen::Board => view! { <super::board::Board/> }.into_any(),
        Screen::Task => view! { <super::task::TaskPage/> }.into_any(),
        Screen::Planner => view! { <super::planner::Planner/> }.into_any(),
        Screen::Brain => view! { <super::brain::Brain/> }.into_any(),
        Screen::Settings => view! { <super::settings::Settings/> }.into_any(),
        Screen::Admin => view! { <super::admin::Admin/> }.into_any(),
    }
}
