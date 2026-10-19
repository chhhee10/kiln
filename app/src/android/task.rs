//! Task detail (pushed): top app bar, title, hero card with the progress
//! ring, vertical timeline, QA captures carousel, stat tiles, reviewer
//! verdict and the bottom app bar (Retry · Escalate · Feedback FAB).

use super::feed::{self, status_label};
use super::nav::Sheet;
use super::use_ui;
use crate::state::data::Data;
use crate::state::derive;
use chrono::{DateTime, Utc};
use kiln_types::api::TaskAction;
use kiln_types::qa::Decision;
use kiln_types::*;
use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub struct Hero {
    pub status: Status,
    /// 0–100 filled part of the ring.
    pub pct: u8,
    /// "48%", "✓", "✕", "!", "—".
    pub big: String,
    pub small: String,
    pub stage_line: String,
    pub attempt_line: String,
}

pub fn hero(d: &Data, t: &Task, now: DateTime<Utc>) -> Hero {
    let st = derive::stages(d, t, now);
    let cur = st
        .iter()
        .find(|s| s.status != Status::Passed)
        .or(st.last())
        .map(|s| s.name)
        .unwrap_or("Builder");
    let status = t.status();
    let passed = st.iter().filter(|s| s.status == Status::Passed).count();
    let pct = match status {
        Status::Running => t.progress,
        Status::Passed => 100,
        _ => (passed * 100 / 3) as u8,
    };
    let big = match status {
        Status::Running => format!("{}%", t.progress),
        Status::Passed => "✓".into(),
        Status::Failed => "✕".into(),
        Status::NeedsHuman => "!".into(),
        Status::Queued => "—".into(),
    };
    let small = if status == Status::Running {
        cur.to_string()
    } else {
        status_label(status).to_string()
    };
    let stage_line = match (status, t.state) {
        (Status::Running, _) => format!("{cur} is working"),
        (Status::Failed, _) => format!("{cur} failed this attempt"),
        (Status::NeedsHuman, _) => "Waiting on your decision".into(),
        (Status::Passed, TaskState::Merged) => "Merged".into(),
        (Status::Passed, _) => "PR ready for review".into(),
        (_, TaskState::Cancelled) => "Stopped".into(),
        _ => "Queued for a slot".into(),
    };
    let cap = t.loop_cap.max(1);
    let mut attempt_line = format!("Attempt {} of {cap}", t.attempt().min(cap));
    if let Some(n) = t.pr_number {
        attempt_line.push_str(&format!(" · PR #{n}"));
    }
    Hero {
        status,
        pct,
        big,
        small,
        stage_line,
        attempt_line,
    }
}

const SHOT_WIDTHS: [u32; 3] = [260, 180, 220];

#[component]
pub fn TaskScreen(key: String) -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let k = key.clone();
    let task = Memo::new(move |_| app.data.with(|d| d.task_by_key(&k).cloned()));
    let id = Memo::new(move |_| task.with(|t| t.as_ref().map(|t| t.id)));
    let detail = Memo::new(move |_| {
        id.get()
            .and_then(|i| app.data.with(|d| d.details.get(&i).cloned()))
    });
    let project = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref().and_then(|t| {
                app.data.with(|d| {
                    d.project(t.project_id)
                        .map(crate::state::util::project_short)
                })
            })
        })
        .unwrap_or_default()
    });
    let hero_vm = Memo::new(move |_| {
        app.tick.track();
        task.with(|t| {
            t.as_ref()
                .map(|t| app.data.with(|d| hero(d, t, Utc::now())))
        })
    });
    let stages = Memo::new(move |_| {
        app.tick.track();
        task.with(|t| {
            t.as_ref()
                .map(|t| app.data.with(|d| derive::stages(d, t, Utc::now())))
                .unwrap_or_default()
        })
    });
    let shots = Memo::new(move |_| {
        detail.with(|d| {
            d.as_ref()
                .map(|d| {
                    d.artifacts
                        .iter()
                        .filter(|a| {
                            matches!(a.kind, ArtifactKind::Screenshot | ArtifactKind::Video)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let back = move |_| {
        ui.nav.update(|n| {
            n.back();
        })
    };
    let with_id = move |f: &dyn Fn(kiln_types::ids::TaskId)| {
        if let Some(i) = id.get_untracked() {
            f(i)
        }
    };
    let retry = move |_| with_id(&|i| app.task_action(i, TaskAction::Retry));
    let escalate = move |_| with_id(&|i| app.task_action(i, TaskAction::Escalate { note: None }));
    let feedback = move |_| with_id(&|i| ui.nav.update(|n| n.open_sheet(Sheet::Feedback(i))));
    let more = move |_| with_id(&|i| ui.nav.update(|n| n.open_sheet(Sheet::More(i))));

    view! {
        <div class="a-page a-slide">
            <div class="a-appbar">
                <button class="a-icon-btn" aria-label="Back" on:click=back>"←"</button>
                <span class="a-appbar-id">{format!("{key} · ")}{project}</span>
                <button class="a-icon-btn right" aria-label="More" on:click=more>"⋮"</button>
            </div>
            <div class="a-scroll a-task">
                {move || task.get().is_none().then(|| view! { <div class="a-nothing">"This task isn't on the board any more."</div> })}
                <div class="a-task-title">{move || task.with(|t| t.as_ref().map(|t| t.title.clone()).unwrap_or_default())}</div>
                {move || hero_vm.get().map(|h| {
                    let c = h.status.hex();
                    view! {
                        <div class="a-card a-hero">
                            <div class="a-ring" style=format!("background:conic-gradient({c} {}%, #32352c 0)", h.pct)>
                                <div class="in">
                                    <span class="big" style=format!("color:{c}")>{h.big.clone()}</span>
                                    <span class="small">{h.small.clone()}</span>
                                </div>
                            </div>
                            <div class="a-hero-text">
                                <span class="a-stchip" style=format!("background:{c}24;color:{c}")><span class="g">{h.status.glyph()}</span>{status_label(h.status)}</span>
                                <span class="line">{h.stage_line.clone()}</span>
                                <span class="att">{h.attempt_line.clone()}</span>
                            </div>
                        </div>
                    }
                })}
                <div class="a-timeline">
                    {move || stages.get().into_iter().map(|s| {
                        let c = s.status.hex();
                        let bg = format!("{c}{}", if s.status == Status::Queued { "14" } else { "24" });
                        let glyph = if s.status == Status::Running { feed::spin(app.tick.get_untracked(), 0) } else { s.status.glyph() };
                        let line = if s.last { "transparent" } else if s.connector_done { Status::Passed.hex() } else { "#43473a" };
                        let dur = match s.status {
                            Status::Running => format!("{}%", s.progress),
                            Status::Queued => "—".into(),
                            _ => s.dur.split(" · ").last().unwrap_or("").replace("failed after ", ""),
                        };
                        view! {
                            <div class="a-tl-row">
                                <div class="a-tl-rail">
                                    <span class="a-tl-tile" style=format!("background:{bg};color:{c}")>
                                        {glyph}
                                        {(s.status == Status::Running).then(|| view! { <span class="pulse" style=format!("border-color:{c}")></span> })}
                                    </span>
                                    <span class="a-tl-line" style=format!("background:{line}")></span>
                                </div>
                                <div class="a-tl-text">
                                    <div class="top">
                                        <span class="n">{s.name}</span>
                                        <span class="l" style=format!("color:{c}")>{status_label(s.status)}</span>
                                        <span class="d">{dur}</span>
                                    </div>
                                    <span class="h">{s.harness_model.clone()}</span>
                                </div>
                            </div>
                        }
                    }).collect_view()}
                </div>
                <div class="a-captures">
                    <div class="a-section-label">"QA captures"</div>
                    {move || {
                        let list = shots.get();
                        if list.is_empty() {
                            return view! { <div class="a-note pad0">"No captures yet. QA attaches screenshots and a session video here."</div> }.into_any();
                        }
                        view! {
                            <div class="a-carousel">
                                {list.into_iter().enumerate().map(|(i, a)| {
                                    let name = a.caption.clone().unwrap_or_else(|| a.path.rsplit('/').next().unwrap_or(&a.path).to_string());
                                    let name = name.split(" · ").next().unwrap_or("").to_string();
                                    let url = if app.is_demo() { None } else { app.client().map(|c| c.artifact_url(a.run_id, a.id)) };
                                    let video = a.kind == ArtifactKind::Video;
                                    let style = format!(
                                        "width:{}px;{}",
                                        SHOT_WIDTHS[i % 3],
                                        url.as_ref().filter(|_| !video).map(|u| format!("background-image:url('{u}'),repeating-linear-gradient(135deg,#22251e 0 8px,#1b1d17 8px 16px);background-size:cover")).unwrap_or_default()
                                    );
                                    let title = name.clone();
                                    let u2 = url.clone();
                                    view! {
                                        <div class="a-shot" style=style on:click=move |_| {
                                            if video {
                                                if let Some(u) = &u2 { crate::state::platform::open_url(u) } else { ui.snack("Videos open on desktop") }
                                            } else {
                                                ui.nav.update(|n| n.open_sheet(Sheet::Shot { title: title.clone(), url: u2.clone() }))
                                            }
                                        }>
                                            {video.then(|| view! { <span class="a-play">"▶"</span> })}
                                            <span>{name}</span>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }}
                </div>
                {move || detail.get().map(|d| {
                    let tests = d.qa_report.as_ref().map(|q| q.tests.clone());
                    let failing_suite = tests.as_ref().and_then(|t| t.suites.iter().find(|s| s.failed > 0).map(|s| s.name.rsplit('/').next().unwrap_or(&s.name).trim_end_matches(".ts").to_string()));
                    let diff = d.diff.clone();
                    view! {
                        <div class="a-stats">
                            <div class="a-stat">
                                <span class="l">"Tests"</span>
                                {match &tests {
                                    Some(t) => view! {
                                        <span class="v">{t.passed}<span class="of">{format!("/{}", t.total())}</span></span>
                                        {if t.failed > 0 {
                                            view! { <span class="s red">{format!("{} failing{}", t.failed, failing_suite.clone().map(|s| format!(" · {s}")).unwrap_or_default())}</span> }.into_any()
                                        } else {
                                            view! { <span class="s green">"all passing"</span> }.into_any()
                                        }}
                                    }.into_any(),
                                    None => view! { <span class="v dim">"—"</span><span class="s">"QA hasn't run yet"</span> }.into_any(),
                                }}
                            </div>
                            <div class="a-stat">
                                <span class="l">"Diff"</span>
                                {match &diff {
                                    Some(x) => view! {
                                        <span class="v green">{format!("+{}", x.additions)}</span>
                                        <span class="s"><span class="red">{format!("−{}", x.deletions)}</span>{format!(" · {} file{}", x.files.len(), if x.files.len() == 1 { "" } else { "s" })}</span>
                                    }.into_any(),
                                    None => view! { <span class="v dim">"—"</span><span class="s">"No commits yet"</span> }.into_any(),
                                }}
                            </div>
                        </div>
                        <div class="a-card a-verdict">
                            <span class="l">"Reviewer verdict"</span>
                            {match d.verdict.clone() {
                                Some(v) if v.decision == Decision::Approve => view! { <span class="h green">"✓ Approved"</span><span class="b">{v.summary}</span> }.into_any(),
                                Some(v) => view! { <span class="h red">"✕ Changes requested"</span><span class="b">{v.summary}</span> }.into_any(),
                                None => view! { <span class="h dim">"Not reviewed yet"</span><span class="b">"The Reviewer runs after QA passes."</span> }.into_any(),
                            }}
                        </div>
                    }
                })}
            </div>
            <div class="a-bottombar">
                <button class="a-text-btn" on:click=retry><span class="g">"↻"</span>"Retry"</button>
                <button class="a-text-btn warn" on:click=escalate><span class="g">"!"</span>"Escalate"</button>
                <button class="a-efab" on:click=feedback>"Feedback"</button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;

    #[test]
    fn hero_for_states() {
        let now = Utc::now();
        let d = demo::seed(now);
        let h = hero(&d, d.task_by_key("FP-142").unwrap(), now);
        assert_eq!(
            (
                h.big.as_str(),
                h.small.as_str(),
                h.stage_line.as_str(),
                h.pct
            ),
            ("48%", "QA", "QA is working", 48)
        );
        assert!(h.attempt_line.starts_with("Attempt 2 of 3 · PR #"));
        let f = hero(&d, d.task_by_key("FP-139").unwrap(), now);
        assert_eq!(
            (f.big.as_str(), f.stage_line.as_str(), f.pct),
            ("✕", "QA failed this attempt", 33)
        );
        let e = hero(&d, d.task_by_key("FP-136").unwrap(), now);
        assert_eq!(
            (e.big.as_str(), e.small.as_str(), e.stage_line.as_str()),
            ("!", "Needs you", "Waiting on your decision")
        );
        let r = hero(&d, d.task_by_key("FP-133").unwrap(), now);
        assert_eq!(
            (r.big.as_str(), r.pct, r.stage_line.as_str()),
            ("✓", 100, "PR ready for review")
        );
    }
}
