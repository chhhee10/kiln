//! Shared desktop components: status pill and
//! dots, avatars, the 3-stage worker timeline, log pane, QA artifacts, tests,
//! reviewer verdict and diff summary.

use crate::state::derive::{self, StageVm};
use crate::state::util::*;
use crate::state::*;
use kiln_types::event::LogLevel;
use kiln_types::qa::Decision;
use kiln_types::*;
use leptos::prelude::*;

pub fn var(s: Status) -> &'static str {
    s.css_var()
}

/// 7px status dot; running adds the pulse ring.
pub fn status_dot(status: impl Fn() -> Status + Send + Sync + Copy + 'static) -> impl IntoView {
    view! {
        <span class="dot7">
            <span style=move || format!("background:var(--{})", var(status()))></span>
            {move || (status() == Status::Running).then(|| view! { <span class="ring" style="background:var(--running)"></span> })}
        </span>
    }
}

/// Desktop status pill: tint background, 6px dot, label.
pub fn status_pill(status: impl Fn() -> Status + Send + Sync + Copy + 'static) -> impl IntoView {
    view! {
        <span class="pill" style=move || { let v = var(status()); format!("background:var(--{v}-tint);color:var(--{v})") }>
            <span class="dot" style=move || format!("background:var(--{})", var(status()))></span>
            {move || status().label()}
        </span>
    }
}

pub fn avatar(initials: String, size: u32, font: f32) -> impl IntoView {
    view! { <span class="avatar" style=format!("width:{size}px;height:{size}px;font-size:{font}px")>{initials}</span> }
}

/// Key hint inside buttons ("F", "⌘↵").
pub fn k(label: &'static str) -> impl IntoView {
    view! { <span class="k">{label}</span> }
}

pub fn mod_key() -> &'static str {
    crate::state::keys::mod_label(platform::is_mac())
}

// ---------------------------------------------------------------- timeline

pub fn timeline(stages: Memo<Vec<StageVm>>, big: bool) -> impl IntoView {
    let app = use_app();
    let spin = move || SPINNER[(app.tick.get() % 10) as usize];
    let stage = move |i: usize| {
        let s = move || stages.with(|v| v.get(i).cloned());
        let status = move || s().map(|x| x.status).unwrap_or(Status::Queued);
        view! {
            <div class="stage">
                <div class="nodeline">
                    <span class="node" style=move || { let v = var(status()); format!("border-color:var(--{v});color:var(--{v})") }>
                        {move || if status() == Status::Running { spin().to_string() } else { status().glyph().to_string() }}
                        {move || (status() == Status::Running).then(|| view! { <span class="ring" style="border-color:var(--running)"></span> })}
                    </span>
                    <span class=move || {
                        let s = s();
                        match s {
                            Some(s) if s.last => "connector none",
                            Some(s) if s.connector_done => "connector done",
                            _ => "connector",
                        }
                    }></span>
                </div>
                <div class="nm">
                    <b>{move || s().map(|x| x.name).unwrap_or_default()}</b>
                    <span style=move || format!("color:var(--{})", var(status()))>{move || s().map(|x| x.label).unwrap_or_default()}</span>
                </div>
                <div class="dur">
                    {move || big.then(|| view! { {s().map(|x| x.harness_model).unwrap_or_default()}<br/> })}
                    {move || s().map(|x| x.dur).unwrap_or_default()}
                </div>
                <div class="sbar">
                    <div style=move || format!("width:{}%;background:var(--{})", s().map(|x| x.progress).unwrap_or(0), var(status()))></div>
                </div>
            </div>
        }
    };
    view! {
        <div class=if big { "timeline big card" } else { "timeline" }>
            {stage(0)}{stage(1)}{stage(2)}
        </div>
    }
}

// ---------------------------------------------------------------- log pane

pub fn log_pane(task: Memo<Option<Task>>, big: bool) -> impl IntoView {
    let app = use_app();
    let tab = move || {
        app.log_tab
            .get()
            .or_else(|| task.with(|t| t.as_ref().map(derive::current_role)))
            .unwrap_or(WorkerRole::Builder)
    };
    let live = move || {
        task.with(|t| {
            t.as_ref().is_some_and(|t| {
                t.state.is_active() && Some(derive::current_role(t)) == Some(tab())
            })
        })
    };
    let n = 14;
    let _ = big;
    let lines = Memo::new(move |_| {
        let role = tab();
        task.with(|t| t.as_ref().map(|t| t.id))
            .map(|id| app.data.with(|d| derive::log_tail(d, id, role, n)))
            .unwrap_or_default()
    });
    let tab_view = move |role: WorkerRole| {
        view! {
            <span class=move || if tab() == role { "log-tab on" } else { "log-tab" } on:click=move |_| app.log_tab.set(Some(role))>
                {role.as_str()}
            </span>
        }
    };
    view! {
        <div class=if big { "logpane big" } else { "logpane" }>
            <div class="log-hd">
                <span class="ttl">"Logs"</span>
                {tab_view(WorkerRole::Builder)}
                {tab_view(WorkerRole::Qa)}
                {tab_view(WorkerRole::Reviewer)}
                <span class="log-state">
                    <span class="dot breathe" style=move || format!("background:var(--{})", if live() { "running" } else { "queued" })></span>
                    {move || if live() { "following" } else { "complete" }}
                </span>
            </div>
            <div class="log-body">
                {move || lines.with(|l| l.is_empty()).then(|| view! { <div class="log-line log-empty">"No output yet"</div> })}
                <For each=move || lines.get() key=|l| (l.run_id, l.seq) let:l>
                    <div class="log-line">
                        <span class="ts">{fmt_hhmmss(l.ts)}</span>
                        <span class=format!("lvl {}", l.level.as_str()) style=format!("color:var(--{})", derive::level_var(l.level))>{l.level.as_str()}</span>
                        <span class=if l.level == LogLevel::Fail { "msg fail" } else { "msg" }>{l.message.clone()}</span>
                    </div>
                </For>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------- QA artifacts

pub fn qa_artifacts(detail: Memo<Option<TaskDetail>>, big: bool) -> impl IntoView {
    let app = use_app();
    let shots = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref()
                .map(|d| {
                    d.artifacts
                        .iter()
                        .filter(|a| a.kind == ArtifactKind::Screenshot)
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let video = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref().and_then(|d| {
                d.artifacts
                    .iter()
                    .find(|a| a.kind == ArtifactKind::Video)
                    .cloned()
            })
        })
    });
    let url_for = move |a: &Artifact| -> Option<String> {
        app.client().map(|c| c.artifact_url(a.run_id, a.id))
    };
    let name = |a: &Artifact| {
        a.caption
            .clone()
            .unwrap_or_else(|| a.path.rsplit('/').next().unwrap_or(&a.path).to_string())
    };
    let summary = move || {
        let n = shots.with(|s| s.len());
        let v = video.with(|v| v.is_some());
        let mut parts = vec![format!("{n} screenshot{}", if n == 1 { "" } else { "s" })];
        if v {
            parts.push("0:48 video".into());
        }
        parts.push("qa.log".into());
        parts.join(" · ")
    };
    let empty = move || shots.with(|s| s.is_empty()) && video.with(|v| v.is_none());
    view! {
        <div class=if big { "big-qa" } else { "" } style="display:flex;flex-direction:column;gap:8px;min-width:0">
            <div class="section-label">
                "QA artifacts"
                {big.then(|| view! { <span class="ml-auto mono" style="font-size:11px;color:var(--dim)">{summary}</span> })}
            </div>
            {move || if empty() {
                view! { <div class="none-yet">"No artifacts yet. QA attaches screenshots and a session video here."</div> }.into_any()
            } else {
                view! {
                    <div class="shots">
                        <For each=move || shots.get() key=|a| a.id let:a>
                            {
                                let nm = name(&a);
                                let url = url_for(&a);
                                // The stripes stay underneath: they show while the image loads
                                // and if it fails.
                                let style = url
                                    .as_ref()
                                    .map(|u| format!("background-image:url('{u}'),repeating-linear-gradient(135deg,var(--stripe-a) 0 6px,var(--stripe-b) 6px 12px)"))
                                    .unwrap_or_default();
                                let cls = if url.is_some() { "shot" } else { "shot stripes" };
                                let lb = Lightbox { title: nm.clone(), url: url.clone(), video: false };
                                view! {
                                    <div class=cls style=style tabindex="0" on:click=move |_| app.lightbox.set(Some(lb.clone()))>
                                        <span class="lbl">{nm.clone()}</span>
                                    </div>
                                }
                            }
                        </For>
                        {move || video.get().map(|v| {
                            let cap = v.caption.clone().unwrap_or_else(|| "session.webm".into());
                            let short = cap.split(" · ").take(2).collect::<Vec<_>>().join(" · ");
                            let label = if big { cap.clone() } else { short };
                            let lb = Lightbox { title: cap.split(" · ").take(2).collect::<Vec<_>>().join(" · "), url: url_for(&v), video: true };
                            view! {
                                <div class="video stripes" tabindex="0" on:click=move |_| app.lightbox.set(Some(lb.clone()))>
                                    <span class="play">"▶"</span>
                                    <span>{label}</span>
                                </div>
                            }
                        })}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ---------------------------------------------------------------- tests

pub fn tests_block(detail: Memo<Option<TaskDetail>>, big: bool) -> impl IntoView {
    let app = use_app();
    let tests = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref()
                .and_then(|d| d.qa_report.as_ref().map(|r| r.tests.clone()))
        })
    });
    let body = move || {
        let Some(t) = tests.get() else {
            return view! { <div class="none-yet">"Tests run once QA starts."</div> }.into_any();
        };
        let rows = t
            .suites
            .iter()
            .map(|s| {
                let running = s.passed == 0 && s.failed == 0 && s.duration_ms == 0;
                let st = if running {
                    Status::Running
                } else if s.failed > 0 {
                    Status::Failed
                } else {
                    Status::Passed
                };
                let d = if running {
                    "…".to_string()
                } else {
                    format!("{} · {:.1}s", s.passed + s.failed, s.duration_ms as f64 / 1000.0)
                };
                let name = s.name.clone();
                view! {
                    <div class="test-row">
                        <span class="g" style=format!("color:var(--{})", var(st))>
                            {move || if st == Status::Running { SPINNER[(app.tick.get() % 10) as usize] } else { st.glyph() }}
                        </span>
                        <span class="n">{name}</span>
                        {big.then(|| view! { <span class="d">{d.clone()}</span> })}
                    </div>
                }
            })
            .collect_view();
        let fail_flex = if t.failed > 0 { 1.5 } else { 0.0 };
        let run_flex = if t.running > 0 { 1.5 } else { 0.0 };
        let assertion = t.failing_assertion.clone();
        view! {
            <div class="segbar">
                <div style=format!("flex:{};background:var(--passed)", t.passed.max(1))></div>
                <div style=format!("flex:{fail_flex};background:var(--failed)")></div>
                <div class="breathe" style=format!("flex:{run_flex};background:var(--running)")></div>
            </div>
            {rows}
            {(big && t.failed > 0).then(|| assertion.map(|a| view! { <div class="fail-box">{a}</div> }))}
        }
        .into_any()
    };
    let summary = move || {
        tests.get().map(|t| {
            if big {
                view! {
                    <span class="ml-auto mono" style="font-size:11px">
                        <span class="c-passed">{format!("{} passed", t.passed)}</span>" · "
                        <span class="c-failed">{format!("{} failed", t.failed)}</span>" · "
                        <span class="c-running">{format!("{} running", t.running)}</span>
                    </span>
                }
                .into_any()
            } else {
                view! {
                    <span class="ml-auto mono" style="font-size:11px">
                        <span class="c-passed">{t.passed}</span>" · "
                        <span class="c-failed">{t.failed}</span>" · "
                        <span class="c-running">{t.running}</span>
                    </span>
                }
                .into_any()
            }
        })
    };
    if big {
        view! {
            <div class="box big big-tests">
                <div class="section-label">"Tests"{summary}</div>
                {body}
            </div>
        }
        .into_any()
    } else {
        view! {
            <div style="display:flex;flex-direction:column;gap:8px;min-width:0">
                <div class="section-label">"Tests"{summary}</div>
                {body}
            </div>
        }
        .into_any()
    }
}

// ---------------------------------------------------------------- reviewer + diff

pub fn reviewer_box(detail: Memo<Option<TaskDetail>>, big: bool) -> impl IntoView {
    let verdict = Memo::new(move |_| detail.with(|d| d.as_ref().and_then(|d| d.verdict.clone())));
    let run = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref().and_then(|d| {
                d.runs
                    .iter()
                    .filter(|r| r.role == WorkerRole::Reviewer)
                    .max_by_key(|r| r.attempt)
                    .cloned()
            })
        })
    });
    // Before the reviewer of this attempt runs, the verdict shown is from the
    // previous attempt: fall back to the task's reviewer choice.
    let fallback = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref().map(|d| {
                let model = d
                    .task
                    .workers
                    .get(&WorkerRole::Reviewer)
                    .map(|w| w.model.clone())
                    .unwrap_or_default();
                (d.task.attempt().saturating_sub(1).max(1), model)
            })
        })
    });
    let attempt = move || {
        run.with(|r| r.as_ref().map(|r| r.attempt))
            .or_else(|| fallback.with(|f| f.as_ref().map(|f| f.0)))
            .unwrap_or(1)
    };
    let head = move || {
        if big {
            view! {
                <div class="section-label">
                    "Reviewer verdict"
                    <span class="ml-auto mono" style="font-size:11px;color:var(--dim)">
                        {move || run.with(|r| r.as_ref().map(|r| (r.attempt, r.model.clone())))
                            .or_else(|| fallback.get())
                            .map(|(a, m)| if m.is_empty() { format!("attempt {a}") } else { format!("attempt {a} · {}", short_model(&m)) })
                            .unwrap_or_default()}
                    </span>
                </div>
            }
            .into_any()
        } else {
            view! { <div class="section-label">{move || format!("Reviewer · attempt {}", attempt())}</div> }.into_any()
        }
    };
    view! {
        <div class=if big { "box big" } else { "box" } style=if big { "gap:8px" } else { "" }>
            {head}
            {move || match verdict.get() {
                Some(v) => {
                    let (st, label) = if v.decision == Decision::Approve { (Status::Passed, "Approved") } else { (Status::Failed, "Changes requested") };
                    view! {
                        <div class="verdict" style=format!("color:var(--{})", var(st))><span class="glyph">{st.glyph()}</span>{label}</div>
                        <div class="verdict-txt">{v.summary.clone()}</div>
                    }.into_any()
                }
                None => view! { <div class="none-yet">"No verdict yet. The Reviewer runs after QA passes."</div> }.into_any(),
            }}
        </div>
    }
}

pub fn diff_box(detail: Memo<Option<TaskDetail>>, big: bool) -> impl IntoView {
    let app = use_app();
    let diff = Memo::new(move |_| detail.with(|d| d.as_ref().and_then(|d| d.diff.clone())));
    let task = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| d.task.clone())));
    let head = move || {
        diff.get().map(|d| {
            view! {
                <span class="ml-auto mono" style="font-size:11px">
                    <span class="c-passed">{format!("+{}", d.additions)}</span>" "
                    <span class="c-failed">{format!("−{}", d.deletions)}</span>
                    {big.then(|| format!(" · {} files", d.files.len()))}
                </span>
            }
        })
    };
    let pr_label = move || {
        task.with(|t| match t {
            Some(t) => match t.pr_number {
                Some(n) if t.status() == Status::Passed => format!("Open PR #{n} on GitHub ↗"),
                Some(n) => format!("Open draft PR #{n} on GitHub ↗"),
                None => "No PR yet".to_string(),
            },
            None => String::new(),
        })
    };
    view! {
        <div class=if big { "box big" } else { "box" } style=if big { "gap:6px" } else { "gap:5px" }>
            <div class="section-label" style=if big { "margin-bottom:4px" } else { "" }>{if big { "Diff summary" } else { "Diff" }}{head}</div>
            {move || match diff.get() {
                Some(d) => d.files.iter().take(5).map(|f| {
                    let (a, dl) = (format!("+{}", f.additions), format!("−{}", f.deletions));
                    view! {
                        <div class="diff-row">
                            <span class="n">{f.path.clone()}</span>
                            <span class="a">{a}</span>
                            {big.then(|| view! { <span class="d">{dl.clone()}</span> })}
                        </div>
                    }
                }).collect_view().into_any(),
                None => view! { <div class="none-yet">"No commits yet."</div> }.into_any(),
            }}
            {big.then(|| view! {
                <a class="pr-btn" on:click=move |ev| { ev.prevent_default(); if let Some(t) = task.get_untracked() { app.open_pr(t.id) } }>{pr_label}</a>
            })}
        </div>
    }
}

/// "Draft PR #418 ↗" / "PR #433 ready for review ↗".
pub fn pr_label(t: &Task) -> String {
    match t.pr_number {
        Some(n) if t.state == TaskState::Merged => format!("PR #{n} merged ↗"),
        Some(n) if t.status() == Status::Passed => format!("PR #{n} ready for review ↗"),
        Some(n) => format!("Draft PR #{n} ↗"),
        None => "No PR yet".into(),
    }
}
