//! Runs: the compact board. Large title, summary, M3 segmented button
//! (Build | QA | Review | Ready) and 72dp list items → Task.

use super::feed::{self, status_label};
use super::use_ui;
use crate::state::data::Data;
use kiln_types::*;
use leptos::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    Build,
    Qa,
    Review,
    Ready,
}

impl Segment {
    pub const ALL: [Segment; 4] = [Segment::Build, Segment::Qa, Segment::Review, Segment::Ready];
    pub fn label(self) -> &'static str {
        match self {
            Segment::Build => "Build",
            Segment::Qa => "QA",
            Segment::Review => "Review",
            Segment::Ready => "Ready",
        }
    }
    pub fn column(self) -> BoardColumn {
        match self {
            Segment::Build => BoardColumn::Building,
            Segment::Qa => BoardColumn::Qa,
            Segment::Review => BoardColumn::Review,
            Segment::Ready => BoardColumn::Ready,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunRow {
    pub key: String,
    pub title: String,
    pub status: Status,
    pub meta: String,
    pub progress: Option<u8>,
    pub trailing: String,
}

/// "4 live · 2 queued · 2 ready for review".
pub fn summary(d: &Data) -> String {
    let live = d.tasks.iter().filter(|t| t.state.is_active()).count();
    let queued = d
        .tasks
        .iter()
        .filter(|t| t.column() == BoardColumn::Planned)
        .count();
    let ready = d
        .tasks
        .iter()
        .filter(|t| t.column() == BoardColumn::Ready)
        .count();
    format!("{live} live · {queued} queued · {ready} ready for review")
}

pub fn rows(d: &Data, seg: Segment) -> Vec<RunRow> {
    d.tasks
        .iter()
        .filter(|t| t.column() == seg.column())
        .map(|t| {
            let status = t.status();
            let running = t.state.is_active();
            let cap = t.loop_cap.max(1);
            RunRow {
                key: t.key.clone(),
                title: t.title.clone(),
                status,
                meta: if running {
                    format!("attempt {}/{cap}", t.attempt().min(cap))
                } else if t.state == TaskState::Cancelled {
                    "Stopped".into()
                } else {
                    status_label(status).into()
                },
                progress: running.then_some(t.progress),
                trailing: if running {
                    format!("{}%", t.progress)
                } else {
                    t.pr_number.map(|n| format!("#{n}")).unwrap_or_default()
                },
            }
        })
        .collect()
}

#[component]
pub fn Runs() -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    // Open on QA (the design's default) unless it's empty and another stage
    // has work, so "Nothing here" never hides ready PRs.
    let first = app.data.with_untracked(|d| {
        if !rows(d, Segment::Qa).is_empty() {
            Segment::Qa
        } else {
            Segment::ALL
                .into_iter()
                .find(|s| !rows(d, *s).is_empty())
                .unwrap_or(Segment::Qa)
        }
    });
    let seg = RwSignal::new(first);
    let summary = Memo::new(move |_| app.data.with(summary));
    let list = Memo::new(move |_| app.data.with(|d| rows(d, seg.get())));
    let keys =
        Memo::new(move |_| list.with(|l| l.iter().map(|r| r.key.clone()).collect::<Vec<_>>()));
    view! {
        <div class="a-page a-fade">
            <div class="a-large">
                <div class="a-large-title">"Runs"</div>
                <div class="a-large-sub">{summary}</div>
            </div>
            <div class="a-seg">
                {Segment::ALL.into_iter().map(|s| {
                    let on = move || seg.get() == s;
                    view! {
                        <button class="a-seg-btn" class:on=on on:click=move |_| seg.set(s)>
                            <span class="chk">{move || if on() { "✓" } else { "" }}</span>{s.label()}
                        </button>
                    }
                }).collect_view()}
            </div>
            <div class="a-list">
                <For each=move || keys.get() key=|k| k.clone() let:k>
                    {
                        let k2 = k.clone();
                        let row = Memo::new(move |_| list.with(|l| l.iter().find(|r| r.key == k2).cloned()));
                        let salt = k.len();
                        let k3 = k.clone();
                        view! {
                            <div class="a-li" on:click=move |_| ui.open_task(&k3)>
                                {move || row.get().map(|r| {
                                    let c = r.status.hex();
                                    let glyph = move || if r.status == Status::Running { feed::spin(app.tick.get(), salt) } else { r.status.glyph() };
                                    view! {
                                        <div class="a-li-tile" style=format!("background:{c}24;color:{c}")>{glyph}</div>
                                        <div class="a-li-text">
                                            <div class="t">{r.title.clone()}</div>
                                            <div class="m"><span class="mono">{r.key.clone()}</span>{format!(" · {}", r.meta)}</div>
                                            {r.progress.map(|p| view! { <div class="a-bar thin"><div style=format!("width:{p}%")></div></div> })}
                                        </div>
                                        <span class="a-li-trail">{r.trailing.clone()}</span>
                                    }
                                })}
                            </div>
                        }
                    }
                </For>
                <Show when=move || keys.with(|k| k.is_empty())>
                    <div class="a-nothing">"Nothing here."</div>
                </Show>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;

    #[test]
    fn segments_and_rows() {
        let d = demo::seed(chrono::Utc::now());
        assert_eq!(summary(&d), "4 live · 2 queued · 2 ready for review");
        let qa = rows(&d, Segment::Qa);
        assert_eq!(qa.len(), 2);
        let r = qa.iter().find(|r| r.key == "FP-142").unwrap();
        assert_eq!(
            (r.meta.as_str(), r.trailing.as_str(), r.progress),
            ("attempt 2/3", "48%", Some(48))
        );
        let f = qa.iter().find(|r| r.key == "FP-139").unwrap();
        assert_eq!(f.meta, "Failed");
        assert!(f.trailing.starts_with('#'));
        let review = rows(&d, Segment::Review);
        assert!(review.iter().any(|r| r.meta == "Needs you"));
        assert!(
            rows(&d, Segment::Ready)
                .iter()
                .all(|r| r.status == Status::Passed && r.progress.is_none())
        );
    }
}
