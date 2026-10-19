//! Settings: credentials vault, worker defaults, memory sharing, devices
//! (phone pairing), appearance (light/dark).

use super::components::*;
use crate::state::derive;
use crate::state::util::*;
use crate::state::*;
use kiln_types::api::PairStartResponse;
use kiln_types::memory::Visibility;
use kiln_types::plan::WorkerChoice;
use kiln_types::*;
use leptos::prelude::*;

#[component]
pub fn Settings() -> impl IntoView {
    let app = use_app();
    let tab = Memo::new(move |_| match app.route.get() {
        Route::Settings(t) => t,
        _ => SettingsTab::Credentials,
    });
    view! {
        <header class="topbar"><div class="topbar-title">"Settings"</div></header>
        <div class="settings">
            <div class="side-tabs">
                {SettingsTab::ALL.into_iter().map(|t| view! {
                    <div class=move || if tab.get() == t { "side-tab on" } else { "side-tab" } tabindex="0" on:click=move |_| app.go(Route::Settings(t))>{t.label()}</div>
                }).collect_view()}
            </div>
            <div class="settings-body">
                <div class="settings-inner">
                    {move || match tab.get() {
                        SettingsTab::Credentials => view! { <Credentials/> }.into_any(),
                        SettingsTab::Defaults => view! { <Defaults/> }.into_any(),
                        SettingsTab::Memory => view! { <MemorySharing/> }.into_any(),
                        SettingsTab::Devices => view! { <Devices/> }.into_any(),
                        SettingsTab::Appearance => view! { <Appearance/> }.into_any(),
                    }}
                </div>
            </div>
        </div>
    }
}

#[component]
fn Credentials() -> impl IntoView {
    let app = use_app();
    let creds = Memo::new(move |_| app.data.with(|d| d.credentials.clone()));
    let revealed = RwSignal::new(Vec::<CredentialId>::new());
    view! {
        <div class="set-hd">
            <div>
                <div class="set-title">"Credentials vault"</div>
                <div class="set-sub">"Keys belong to you. Workers you dispatch use them, encrypted at rest on the Mac mini."</div>
            </div>
            <button class="btn btn-secondary btn-md ml-auto" on:click=move |_| app.modal.set(Some(Modal::AddCredential))>"+ Add credential"</button>
        </div>
        <div class="list-card">
            <For each=move || creds.get() key=|c| (c.id, c.masked.clone(), format!("{:?}", c.status)) let:c>
                {
                    let id = c.id;
                    let (st, status) = derive::credential_status(&c);
                    let (hidden, shown) = (derive::credential_hidden(&c), derive::credential_revealed(&c));
                    let is_rev = move || revealed.with(|r| r.contains(&id));
                    view! {
                        <div class="cred-row">
                            <div class="l">
                                <div class="nm"><b>{c.name.clone()}</b><span class="type">{c.kind.label()}</span></div>
                                <div class="val">{move || if is_rev() { shown.clone() } else { hidden.clone() }}</div>
                                <div class="st" style=format!("color:var(--{})", var(status))><span class="dot" style=format!("background:var(--{})", var(status))></span>{st}</div>
                            </div>
                            <div class="btns">
                                <button class="btn btn-secondary btn-sm" on:click=move |_| revealed.update(|r| if let Some(i) = r.iter().position(|x| *x == id) { r.remove(i); } else { r.push(id); })>
                                    {move || if is_rev() { "Hide" } else { "Reveal" }}
                                </button>
                                <button class="btn btn-secondary btn-sm" on:click=move |_| app.test_credential(id)>"Test"</button>
                                <button class="btn btn-secondary btn-sm" on:click=move |_| app.modal.set(Some(Modal::RotateCredential(id)))>"Rotate"</button>
                            </div>
                        </div>
                    }
                }
            </For>
            {move || creds.with(|c| c.is_empty()).then(|| view! { <div class="tbl-empty">"No credentials yet. Add an Anthropic or OpenAI API key, a Claude OAuth token, or your LiteLLM proxy."</div> })}
        </div>
        <div class="note">"Reveal shows the masked value stored in the vault; secrets are never shown again after saving. Provider terms apply: subscription OAuth tokens used in automation may be restricted, API keys are the safe default."</div>
    }
}

fn harness_options(app: AppState) -> Memo<Vec<(String, String)>> {
    Memo::new(move |_| {
        app.data.with(|d| {
            d.harnesses
                .as_ref()
                .map(|c| {
                    c.harnesses
                        .iter()
                        .map(|h| (h.id.clone(), h.label.clone()))
                        .collect()
                })
                .unwrap_or_default()
        })
    })
}

fn model_options(app: AppState) -> Memo<Vec<String>> {
    Memo::new(move |_| {
        app.data.with(|d| {
            let mut v: Vec<String> = Vec::new();
            for m in d
                .harnesses
                .iter()
                .flat_map(|c| c.harnesses.iter())
                .flat_map(|h| h.models.iter())
            {
                if !v.contains(m) {
                    v.push(m.clone());
                }
            }
            v
        })
    })
}

#[component]
fn Defaults() -> impl IntoView {
    let app = use_app();
    let harnesses = harness_options(app);
    let models = model_options(app);
    let row = move |role: WorkerRole, desc: &'static str| {
        let choice = Memo::new(move |_| {
            app.data
                .with(|d| {
                    d.settings
                        .as_ref()
                        .and_then(|s| s.worker_defaults.get(&role).cloned())
                })
                .unwrap_or_else(|| WorkerChoice::new("", ""))
        });
        let set = move |f: Box<dyn FnOnce(&mut WorkerChoice)>| {
            app.update_settings(move |s| {
                let e = s
                    .worker_defaults
                    .entry(role)
                    .or_insert_with(|| WorkerChoice::new("claude_sdk", "claude-sonnet-4.5"));
                f(e);
            })
        };
        view! {
            <div class="defaults-row">
                <div class="l"><b>{role.label()}</b><span>{desc}</span></div>
                <select class="select lg" on:change=move |ev| { let v = event_target_value(&ev); set(Box::new(move |c| c.harness = v)); }>
                    {move || {
                        let cur = choice.with(|c| c.harness.clone());
                        harnesses.get().into_iter().map(|(id, label)| { let sel = id == cur; view! { <option value=id selected=sel>{label}</option> } }).collect_view()
                    }}
                </select>
                <select class="select lg model" on:change=move |ev| { let v = event_target_value(&ev); set(Box::new(move |c| c.model = v)); }>
                    {move || {
                        let cur = choice.with(|c| c.model.clone());
                        let mut opts = models.get();
                        if !cur.is_empty() && !opts.contains(&cur) { opts.push(cur.clone()); }
                        opts.into_iter().map(|m| { let sel = m == cur; view! { <option value=m.clone() selected=sel>{m.clone()}</option> } }).collect_view()
                    }}
                </select>
            </div>
        }
    };
    view! {
        <div class="set-hd"><div>
            <div class="set-title">"Default harness & model"</div>
            <div class="set-sub">"New plans start with these per role. The planner can override them per task, and you can override the planner."</div>
        </div></div>
        <div class="list-card">
            {row(WorkerRole::Builder, "Writes the code")}
            {row(WorkerRole::Qa, "Tests and drives the app")}
            {row(WorkerRole::Reviewer, "Checks the diff against the plan")}
            <div class="opt-row">
                <div class="l"><b>"Loop limit"</b><span>"Builder retries after a QA or Reviewer failure, before escalating to a human"</span></div>
                <div class="stepper">
                    <span class="b" tabindex="0" on:click=move |_| app.set_loop_limit(app.loop_limit.get_untracked().saturating_sub(1))>"−"</span>
                    <span class="v">{move || app.loop_limit.get()}</span>
                    <span class="b" tabindex="0" on:click=move |_| app.set_loop_limit(app.loop_limit.get_untracked() + 1)>"+"</span>
                </div>
            </div>
        </div>
        <div class="note">"Models not hosted by a provider key are routed through your LiteLLM proxy."</div>
    }
}

#[component]
fn MemorySharing() -> impl IntoView {
    let app = use_app();
    let current = Memo::new(move |_| {
        app.data
            .with(|d| d.settings.as_ref().map(|s| s.default_visibility))
            .unwrap_or(Visibility::Private)
    });
    let planner_reads = Memo::new(move |_| {
        app.data
            .with(|d| d.settings.as_ref().map(|s| s.planner_reads_company))
            .unwrap_or(true)
    });
    let opts = [
        (
            Visibility::Private,
            "Private",
            "Only you, and workers on your runs.",
        ),
        (
            Visibility::Shared,
            "Shared with my project teammates",
            "People who own tasks in the same project. You can change this per item.",
        ),
        (
            Visibility::Company,
            "Company",
            "Requires admin approval before anyone else sees it.",
        ),
    ];
    view! {
        <div class="set-hd"><div>
            <div class="set-title">"Memory sharing defaults"</div>
            <div class="set-sub">"Workers save what they learn from your runs. Pick who sees new memories by default."</div>
        </div></div>
        <div style="display:flex;flex-direction:column;gap:8px">
            {opts.into_iter().map(|(v, n, d)| view! {
                <div class=move || if current.get() == v { "radio-card on" } else { "radio-card" } tabindex="0"
                    on:click=move |_| app.update_settings(move |s| s.default_visibility = v)>
                    <span class="radio"><span></span></span>
                    <div class="l"><b>{n}</b><span>{d}</span></div>
                </div>
            }).collect_view()}
        </div>
        <div class="toggle-card" tabindex="0" on:click=move |_| app.update_settings(|s| s.planner_reads_company = !s.planner_reads_company)>
            <div class="l"><b>"Planner can read Company memories"</b><span>"Pulls relevant conventions into new plans automatically"</span></div>
            <span class=move || if planner_reads.get() { "toggle on" } else { "toggle" }><span></span></span>
        </div>
        <div class="note">"Personal preferences are always Private and can't be shared."</div>
    }
}

#[component]
fn Devices() -> impl IntoView {
    let app = use_app();
    let pairing = RwSignal::new(None::<PairStartResponse>);
    let busy = RwSignal::new(false);
    let start = move || {
        busy.set(true);
        app.pair_start(move |r| {
            busy.set(false);
            pairing.set(Some(r));
        });
    };
    let remaining = Memo::new(move |_| {
        app.tick.track();
        pairing.with(|p| {
            p.as_ref()
                .map(|p| (p.expires_at - chrono::Utc::now()).num_seconds().max(0))
        })
    });
    view! {
        <div class="set-hd">
            <div>
                <div class="set-title">"Devices"</div>
                <div class="set-sub">"Pair your Android phone: scan the QR code in the Kiln app, or enter the server URL and the one-time code. Codes expire after 10 minutes."</div>
            </div>
            <button class="btn btn-primary btn-md ml-auto" disabled=move || busy.get() on:click=move |_| start()>
                {move || if pairing.with(|p| p.is_some()) { "New code" } else { "Pair a phone" }}
            </button>
        </div>
        {move || match pairing.get() {
            Some(p) => {
                let uri = p.pair_uri.clone();
                view! {
                    <div class="list-card">
                        <div class="pair">
                            <div class="qr" inner_html=p.qr_svg.clone()></div>
                            <div class="info">
                                <div class="caption">"ONE-TIME CODE"</div>
                                <div class="pair-code">{p.code.clone()}</div>
                                <div class="note">"Server "<span class="mono c-muted">{p.server_url.clone()}</span></div>
                                <div class="note mono">{move || match remaining.get() {
                                    Some(0) => "expired · make a new code".to_string(),
                                    Some(s) => format!("expires in {}", fmt_duration(s)),
                                    None => String::new(),
                                }}</div>
                                <div style="display:flex;gap:6px">
                                    <button class="btn btn-secondary btn-sm" on:click=move |_| { platform::copy_text(&uri); app.toast("Pairing link copied", true); }>"Copy link"</button>
                                </div>
                                {app.is_demo().then(|| view! { <div class="note">"Demo mode: this QR is a placeholder and can't be scanned."</div> })}
                            </div>
                        </div>
                    </div>
                }.into_any()
            }
            None => view! {
                <div class="list-card"><div class="tbl-empty">"No pairing in progress. Push notifications reach paired phones through UnifiedPush / ntfy."</div></div>
            }.into_any(),
        }}
    }
}

#[component]
fn Appearance() -> impl IntoView {
    let app = use_app();
    let card = move |t: Theme,
                     name: &'static str,
                     desc: &'static str,
                     swatch: [&'static str; 4]| {
        view! {
            <div class=move || if app.theme.get() == t { "radio-card on" } else { "radio-card" } tabindex="0" on:click=move |_| app.set_theme(t)
                style="flex-direction:column">
                <div class="theme-swatch" style=format!("background:{}", swatch[0])>
                    <span style=format!("background:{};flex:.6", swatch[1])></span>
                    <span style=format!("background:{}", swatch[2])></span>
                    <span style=format!("background:{};flex:.25", swatch[3])></span>
                </div>
                <div style="display:flex;gap:12px">
                    <span class="radio"><span></span></span>
                    <div class="l"><b>{name}</b><span>{desc}</span></div>
                </div>
            </div>
        }
    };
    view! {
        <div class="set-hd"><div>
            <div class="set-title">"Appearance"</div>
            <div class="set-sub">"Kiln is dark-first. Light mode uses the same roles and status colors, tuned for daylight."</div>
        </div></div>
        <div class="theme-cards">
            {card(Theme::Dark, "Dark", "Graphite. The default.", ["#0f1115", "#0c0e11", "#15171c", "#cdf04a"])}
            {card(Theme::Light, "Light", "Paper background, darker status hues.", ["#f7f7f4", "#efefea", "#ffffff", "#cdf04a"])}
        </div>
    }
}
