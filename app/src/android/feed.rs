//! The Planner feed (pure, unit tested natively): the conversation plus
//! every card that needs a human, assembled from shared `Data` and a little
//! Android-local memory (`FeedLocal`: resolved cards, planner follow-ups,
//! when each card first appeared).
//!
//! Order: a digest band (greeting, LIVE NOW, the cards that were already
//! waiting when the app opened) followed by a chronological stream
//! (messages, the plan card after the message that produced it, planner
//! follow-ups and cards that arrived while the app was open). New items land
//! at the bottom, so the feed auto-scrolls to them.

use crate::state::data::Data;
use crate::state::derive;
use crate::state::util::*;
use chrono::{DateTime, Timelike, Utc};
use kiln_types::ids::*;
use kiln_types::memory::*;
use kiln_types::qa::Decision;
use kiln_types::*;
use std::collections::BTreeMap;

pub const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Identity of a feed item (also its DOM id and highlight target).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CardKey {
    Greeting,
    Live,
    /// A failed/escalated task card at a given attempt (a new failure after
    /// a retry is a new card).
    Task(TaskId, u32),
    Prs,
    Memory(MemoryId),
    Plan(PlanId),
    Msg(MessageId),
    Local(u64),
    // Link targets only (never on an item):
    AnyPlan,
    AnyMemory,
    AnyTask(TaskId),
}

impl CardKey {
    pub fn dom_id(&self) -> String {
        match self {
            CardKey::Greeting => "card-greeting".into(),
            CardKey::Live => "card-live".into(),
            CardKey::Task(t, a) => format!("card-task-{t}-{a}"),
            CardKey::Prs => "card-prs".into(),
            CardKey::Memory(m) => format!("card-mem-{m}"),
            CardKey::Plan(p) => format!("card-plan-{p}"),
            CardKey::Msg(m) => format!("card-msg-{m}"),
            CardKey::Local(n) => format!("card-local-{n}"),
            CardKey::AnyPlan => "card-anyplan".into(),
            CardKey::AnyMemory => "card-anymem".into(),
            CardKey::AnyTask(t) => format!("card-anytask-{t}"),
        }
    }

    /// Does this (possibly wildcard) link target match an item key?
    pub fn matches(&self, item: &CardKey) -> bool {
        match (self, item) {
            (CardKey::AnyPlan, CardKey::Plan(_)) => true,
            (CardKey::AnyMemory, CardKey::Memory(_)) => true,
            (CardKey::AnyTask(a), CardKey::Task(b, _)) => a == b,
            (a, b) => a == b,
        }
    }

    /// Cards that count toward "Needs you" while unresolved.
    pub fn needs_you(&self) -> bool {
        matches!(
            self,
            CardKey::Task(..) | CardKey::Memory(_) | CardKey::Plan(_)
        )
    }

    fn is_card(&self) -> bool {
        matches!(
            self,
            CardKey::Task(..) | CardKey::Prs | CardKey::Memory(_) | CardKey::Plan(_)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskCardKind {
    Failed,
    Escalated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dot {
    pub name: &'static str,
    pub glyph: &'static str,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveRow {
    pub task: TaskId,
    pub key: String,
    pub title: String,
    pub role: &'static str,
    pub progress: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrRow {
    pub task: TaskId,
    pub key: String,
    pub title: String,
    pub pr: Option<u64>,
    pub adds: Option<u64>,
    pub dels: Option<u64>,
    pub project: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanRow {
    pub id: String,
    pub title: String,
    pub models: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FeedKind {
    Agent {
        text: String,
        time: String,
    },
    User {
        text: String,
        meta: String,
    },
    Live {
        used: usize,
        slots: u32,
        avg: u8,
        rows: Vec<LiveRow>,
    },
    TaskCard {
        task: TaskId,
        kind: TaskCardKind,
        key: String,
        title: String,
        label: String,
        time: String,
        dots: Vec<Dot>,
        sub: String,
    },
    Prs {
        rows: Vec<PrRow>,
        time: String,
    },
    Memory {
        id: MemoryId,
        who: String,
        title: String,
        body: String,
        from: &'static str,
        to: &'static str,
    },
    Plan {
        id: PlanId,
        version: u32,
        title: String,
        rows: Vec<PlanRow>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FeedItem {
    pub key: CardKey,
    pub kind: FeedKind,
    /// "✓ …" line replacing the actions once acted on.
    pub resolved: Option<String>,
    /// (band, time ms, tiebreak) — see the module doc.
    pub order: (u8, i64, u32),
}

impl FeedItem {
    pub fn needs_you(&self) -> bool {
        self.key.needs_you() && self.resolved.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalMsg {
    pub id: u64,
    pub text: String,
    pub at: DateTime<Utc>,
}

/// Android-local feed memory.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FeedLocal {
    /// Cards acted on → their "✓" line (without the ✓).
    pub resolved: BTreeMap<CardKey, String>,
    /// Resolved cards as they looked when resolved (kept after the data
    /// stops producing them, e.g. a retried task is no longer failed).
    pub snapshots: BTreeMap<CardKey, FeedItem>,
    /// Planner follow-ups posted after a card was resolved.
    pub msgs: Vec<LocalMsg>,
    /// When each card first appeared: `None` = already there at startup.
    pub seen: BTreeMap<CardKey, Option<i64>>,
    /// Startup snapshot taken: later cards go to the stream.
    pub primed: bool,
    /// Session start, used for the greeting's time.
    pub started: Option<DateTime<Utc>>,
    pub next_id: u64,
}

impl FeedLocal {
    pub fn resolve(&mut self, item: &FeedItem, text: impl Into<String>) {
        let text = text.into();
        let mut snap = item.clone();
        snap.resolved = Some(text.clone());
        self.resolved.insert(item.key.clone(), text);
        self.snapshots.insert(item.key.clone(), snap);
    }

    pub fn say(&mut self, text: impl Into<String>, at: DateTime<Utc>) {
        self.next_id += 1;
        self.msgs.push(LocalMsg {
            id: self.next_id,
            text: text.into(),
            at,
        });
    }

    /// Record first-seen times for cards in `items` (call after assembling).
    /// Returns true if anything changed.
    pub fn register(&mut self, items: &[FeedItem], now: DateTime<Utc>) -> bool {
        let mut changed = false;
        for it in items.iter().filter(|i| i.key.is_card()) {
            if !self.seen.contains_key(&it.key) {
                let v = if self.primed {
                    Some(now.timestamp_millis())
                } else {
                    None
                };
                self.seen.insert(it.key.clone(), v);
                changed = true;
            }
        }
        changed
    }

    /// Cards that disappeared from the data because they were handled
    /// outside this screen (an inline action on a push notification):
    /// keep them, resolved with the notification's "✓ …" line.
    pub fn reconcile(&mut self, prev: &[FeedItem], now_items: &[FeedItem], d: &Data) -> bool {
        let mut changed = false;
        for it in prev
            .iter()
            .filter(|i| i.key.is_card() && i.resolved.is_none())
        {
            if now_items.iter().any(|n| n.key == it.key) {
                continue;
            }
            if let Some(text) = notification_resolution(d, &it.key) {
                self.resolve(it, text);
                changed = true;
            }
        }
        changed
    }
}

/// A resolved notification's text for a card ("✓ Retried …" → "Retried …").
pub fn notification_resolution(d: &Data, key: &CardKey) -> Option<String> {
    d.notifications.iter().find_map(|n| {
        let hit = match key {
            CardKey::Task(t, _) => n.task_id == Some(*t),
            CardKey::Plan(p) => n.plan_id == Some(*p),
            CardKey::Memory(m) => n.memory_id == Some(*m),
            _ => false,
        };
        if !hit {
            return None;
        }
        n.resolved
            .as_ref()
            .map(|r| r.trim_start_matches('✓').trim().to_string())
    })
}

// ---------------------------------------------------------------- formatting

/// "9:38" (local, no leading zero).
pub fn clock(at: DateTime<Utc>) -> String {
    let l = at.with_timezone(&chrono::Local);
    format!("{}:{:02}", l.hour(), l.minute())
}

/// "now", "3m", "2h", "4d".
pub fn short_ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = (now - at).num_seconds().max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn number_word(n: usize) -> String {
    const W: [&str; 10] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    W.get(n)
        .map(|s| s.to_string())
        .unwrap_or_else(|| n.to_string())
}

fn join_and(parts: &[String]) -> String {
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        n => format!("{}, and {}", parts[..n - 1].join(", "), parts[n - 1]),
    }
}

/// Android status label ("Needs you", not "Needs human").
pub fn status_label(s: Status) -> &'static str {
    match s {
        Status::NeedsHuman => "Needs you",
        s => s.label(),
    }
}

/// Spinner frame for a running item.
pub fn spin(tick: u64, salt: usize) -> &'static str {
    SPIN[((tick as usize) + salt) % SPIN.len()]
}

/// "Morning, Alex. Overnight: two PRs are ready for review, FP-139 failed
/// QA, and FP-136 needs a decision from you."
pub fn greeting(d: &Data, items: &[FeedItem], at: DateTime<Utc>) -> String {
    let name =
        d.me.as_ref()
            .map(|u| first_name(&u.name))
            .unwrap_or_default();
    let hour = at.with_timezone(&chrono::Local).hour();
    let (hello, lead) = match hour {
        4..=11 => ("Morning", "Overnight"),
        12..=17 => ("Afternoon", "Since this morning"),
        _ => ("Evening", "Today"),
    };
    let mut parts = Vec::new();
    // PRs first, then failures/decisions in feed order (prototype copy).
    let mut ordered: Vec<&FeedItem> = items.iter().collect();
    ordered.sort_by_key(|i| !matches!(i.kind, FeedKind::Prs { .. }));
    for it in ordered {
        match &it.kind {
            FeedKind::Prs { rows, .. } if !rows.is_empty() => parts.push(if rows.len() == 1 {
                "one PR is ready for review".to_string()
            } else {
                format!("{} PRs are ready for review", number_word(rows.len()))
            }),
            FeedKind::TaskCard {
                key, kind, label, ..
            } => parts.push(match kind {
                TaskCardKind::Failed => {
                    let stage = label
                        .split(" · ")
                        .next()
                        .unwrap_or("")
                        .replacen("failed", "", 1);
                    format!("{key} {}", failed_phrase(stage.trim()))
                }
                TaskCardKind::Escalated => format!("{key} needs a decision from you"),
            }),
            FeedKind::Plan { .. } => parts.push("a plan is waiting for your approval".into()),
            _ => {}
        }
    }
    let mems = items
        .iter()
        .filter(|i| matches!(i.kind, FeedKind::Memory { .. }))
        .count();
    if mems > 0 {
        parts.push(if mems == 1 {
            "a memory needs your approval".into()
        } else {
            format!("{} memories need your approval", number_word(mems))
        });
    }
    let hi = if name.is_empty() {
        format!("{hello}.")
    } else {
        format!("{hello}, {name}.")
    };
    if parts.is_empty() {
        format!("{hi} All quiet. Nothing needs you right now.")
    } else {
        format!("{hi} {lead}: {}.", join_and(&parts))
    }
}

/// "QA" → "failed QA", "Build" → "failed to build".
fn failed_phrase(stage: &str) -> String {
    match stage {
        "" => "failed".into(),
        "Build" => "failed to build".into(),
        "Review" => "failed review".into(),
        s => format!("failed {s}"),
    }
}

// ---------------------------------------------------------------- cards

/// Tasks whose cards this user acts on: own tasks, or all for admins.
fn mine(d: &Data, t: &Task) -> bool {
    d.is_admin() || d.me.as_ref().is_some_and(|u| u.id == t.owner_id)
}

pub fn stage_dots(d: &Data, t: &Task, now: DateTime<Utc>, tick: u64) -> Vec<Dot> {
    derive::stages(d, t, now)
        .into_iter()
        .map(|s| Dot {
            name: s.name,
            glyph: if s.status == Status::Running {
                spin(tick, 0)
            } else {
                s.status.glyph()
            },
            status: s.status,
        })
        .collect()
}

/// Why it failed / what's contested, from the QA report and verdict.
fn failure_reason(d: &Data, t: &Task) -> Option<String> {
    let det = d.details.get(&t.id)?;
    let qa = det.qa_report.as_ref();
    qa.and_then(|q| q.tests.failing_assertion.clone())
        .or_else(|| {
            qa.and_then(|q| {
                q.failures()
                    .next()
                    .map(|c| format!("Acceptance criterion failed: {}", c.criterion))
            })
        })
        .or_else(|| {
            det.verdict
                .as_ref()
                .filter(|v| v.decision == Decision::ChangesRequested)
                .map(|v| v.summary.clone())
        })
}

fn sentence(s: &str) -> String {
    let s = s.trim();
    if s.ends_with(['.', '!', '?']) {
        s.to_string()
    } else {
        format!("{s}.")
    }
}

pub fn task_card(d: &Data, t: &Task, now: DateTime<Utc>, tick: u64) -> Option<FeedItem> {
    let kind = match t.state {
        TaskState::Failed => TaskCardKind::Failed,
        TaskState::Escalated => TaskCardKind::Escalated,
        _ => return None,
    };
    let cap = t.loop_cap.max(1);
    let att = t.attempt().min(cap);
    let label = match kind {
        TaskCardKind::Failed => derive::card_meta(t),
        TaskCardKind::Escalated => format!("Escalated · {att}/{cap}"),
    };
    let reason = failure_reason(d, t);
    let sub = match kind {
        TaskCardKind::Failed => {
            let left = cap.saturating_sub(att);
            let tail = match left {
                0 => "No attempts left: a retry escalates to you if it fails again.".to_string(),
                1 => "The Builder has one attempt left.".to_string(),
                n => format!("The Builder has {} attempts left.", number_word(n as usize)),
            };
            match reason {
                Some(r) => format!("{} {tail}", sentence(&r)),
                None => tail,
            }
        }
        TaskCardKind::Escalated => {
            let what = reason
                .map(|r| sentence(&r))
                .unwrap_or_else(|| "QA and the Reviewer couldn't converge.".into());
            format!("Looped {att} times. {what} Pick one.")
        }
    };
    Some(FeedItem {
        key: CardKey::Task(t.id, t.attempt()),
        kind: FeedKind::TaskCard {
            task: t.id,
            kind,
            key: t.key.clone(),
            title: t.title.clone(),
            label,
            time: short_ago(t.updated_at, now),
            dots: stage_dots(d, t, now, tick),
            sub,
        },
        resolved: None,
        order: (0, 0, if kind == TaskCardKind::Failed { 10 } else { 11 }),
    })
}

pub fn pr_rows(d: &Data) -> Vec<PrRow> {
    let mut v: Vec<&Task> = d
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Ready && mine(d, t))
        .collect();
    v.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
    v.into_iter()
        .map(|t| {
            let diff = d.details.get(&t.id).and_then(|x| x.diff.as_ref());
            PrRow {
                task: t.id,
                key: t.key.clone(),
                title: t.title.clone(),
                pr: t.pr_number,
                adds: diff.map(|x| x.additions),
                dels: diff.map(|x| x.deletions),
                project: d
                    .project(t.project_id)
                    .map(project_short)
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn short_person_name(d: &Data, u: UserId) -> String {
    short_person(&d.user_name(u))
}

pub fn memory_card(d: &Data, m: &MemoryItem) -> FeedItem {
    let to = m.requested_visibility.unwrap_or(Visibility::Company);
    let from = if m.visibility == to {
        if to == Visibility::Company {
            Visibility::Shared
        } else {
            Visibility::Private
        }
    } else {
        m.visibility
    };
    let src = m
        .source_label
        .as_deref()
        .and_then(|s| s.split(" · ").next())
        .filter(|s| !s.is_empty() && *s != "manual")
        .map(|s| format!(" · from {s}"))
        .unwrap_or_default();
    FeedItem {
        key: CardKey::Memory(m.id),
        kind: FeedKind::Memory {
            id: m.id,
            who: format!("{}{src}", short_person_name(d, m.owner_id)),
            title: m.title.clone(),
            body: m.body.clone(),
            from: from.label(),
            to: to.label(),
        },
        resolved: None,
        order: (0, 0, 13),
    }
}

/// "sonnet-4.5 → gpt-5 → opus-4.1".
pub fn plan_models(d: &Data, ts: &kiln_types::plan::TaskSpec) -> String {
    WorkerRole::PIPELINE
        .into_iter()
        .map(|r| {
            ts.workers
                .get(r)
                .cloned()
                .or_else(|| {
                    d.settings
                        .as_ref()
                        .and_then(|s| s.worker_defaults.get(&r).cloned())
                })
                .map(|w| short_model(&w.model))
                .unwrap_or_else(|| "default".into())
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

pub fn plan_card(d: &Data, p: &Plan) -> FeedItem {
    FeedItem {
        key: CardKey::Plan(p.id),
        kind: FeedKind::Plan {
            id: p.id,
            version: p.version,
            title: if p.spec.feature.trim().is_empty() {
                "Untitled plan".into()
            } else {
                p.spec.feature.clone()
            },
            rows: p
                .spec
                .tasks
                .iter()
                .map(|ts| PlanRow {
                    id: ts.id.to_uppercase(),
                    title: ts.title.clone(),
                    models: plan_models(d, ts),
                })
                .collect(),
        },
        resolved: None,
        order: (1, p.updated_at.timestamp_millis(), 1),
    }
}

pub fn live_card(d: &Data) -> Option<FeedItem> {
    let running: Vec<&Task> = d.tasks.iter().filter(|t| t.state.is_active()).collect();
    if running.is_empty() {
        return None;
    }
    let avg = running.iter().map(|t| t.progress as u32).sum::<u32>() / running.len() as u32;
    let mut rows: Vec<LiveRow> = running
        .iter()
        .map(|t| LiveRow {
            task: t.id,
            key: t.key.clone(),
            title: t.title.clone(),
            role: derive::current_role(t).label(),
            progress: t.progress,
        })
        .collect();
    // Pipeline order like the prototype: Builder rows, then QA, then Reviewer.
    rows.sort_by_key(|r| match r.role {
        "Builder" => 0,
        "QA" => 1,
        _ => 2,
    });
    Some(FeedItem {
        key: CardKey::Live,
        kind: FeedKind::Live {
            used: running.len(),
            slots: if d.machine.container_slots > 0 {
                d.machine.container_slots
            } else {
                8
            },
            avg: avg as u8,
            rows,
        },
        resolved: None,
        order: (0, 0, 1),
    })
}

/// The conversation the home screen shows: the given thread, else the most
/// recent thread with a drafting plan, else the most recent thread.
pub fn current_thread(d: &Data, pick: Option<ThreadId>) -> Option<ThreadId> {
    pick.filter(|t| d.thread(*t).is_some())
        .or_else(|| {
            d.threads
                .iter()
                .filter(|t| t.status == PlanStatus::Drafting)
                .max_by_key(|t| t.updated_at)
                .map(|t| t.id)
        })
        .or_else(|| d.threads.first().map(|t| t.id))
}

/// Assemble the whole feed (unfiltered), sorted.
pub fn assemble(
    d: &Data,
    thread: Option<ThreadId>,
    local: &FeedLocal,
    now: DateTime<Utc>,
    tick: u64,
) -> Vec<FeedItem> {
    let mut cards: Vec<FeedItem> = Vec::new();
    let mut tasks: Vec<&Task> = d
        .tasks
        .iter()
        .filter(|t| matches!(t.state, TaskState::Failed | TaskState::Escalated) && mine(d, t))
        .collect();
    tasks.sort_by_key(|t| (t.state == TaskState::Escalated, t.updated_at));
    cards.extend(tasks.into_iter().filter_map(|t| task_card(d, t, now, tick)));
    let prs = pr_rows(d);
    if !prs.is_empty() {
        let newest = d
            .tasks
            .iter()
            .filter(|t| prs.iter().any(|r| r.task == t.id))
            .map(|t| t.updated_at)
            .max()
            .unwrap_or(now);
        cards.push(FeedItem {
            key: CardKey::Prs,
            kind: FeedKind::Prs {
                rows: prs,
                time: short_ago(newest, now),
            },
            resolved: None,
            order: (0, 0, 12),
        });
    }
    let mut mems: Vec<&MemoryItem> = d
        .memory
        .iter()
        .filter(|m| m.status == MemoryStatus::Pending)
        .collect();
    mems.sort_by_key(|m| m.updated_at);
    cards.extend(mems.into_iter().map(|m| memory_card(d, m)));

    // Conversation.
    let thread = current_thread(d, thread);
    let mut stream: Vec<FeedItem> = Vec::new();
    if let Some(tid) = thread {
        let msgs = d
            .thread_details
            .get(&tid)
            .map(|x| x.messages.clone())
            .unwrap_or_default();
        for m in &msgs {
            let ts = m.created_at.timestamp_millis();
            let kind = match m.author {
                MessageAuthor::User => FeedKind::User {
                    text: m.body.clone(),
                    meta: if m.voice {
                        format!("◉ voice · {}", clock(m.created_at))
                    } else {
                        clock(m.created_at)
                    },
                },
                _ => FeedKind::Agent {
                    text: m.body.clone(),
                    time: clock(m.created_at),
                },
            };
            stream.push(FeedItem {
                key: CardKey::Msg(m.id),
                kind,
                resolved: None,
                order: (1, ts, 0),
            });
        }
        if let Some(p) = d
            .plan_for_thread(tid)
            .filter(|p| p.status == PlanStatus::Drafting)
        {
            let mut card = plan_card(d, p);
            // Right after the message that produced this draft.
            let anchor = msgs
                .iter()
                .filter(|m| m.author != MessageAuthor::User)
                .filter(|m| m.plan_version.is_some_and(|v| v <= p.version))
                .max_by_key(|m| m.created_at)
                .or_else(|| {
                    msgs.iter()
                        .filter(|m| m.author != MessageAuthor::User)
                        .max_by_key(|m| m.created_at)
                });
            card.order = (
                1,
                anchor
                    .map(|m| m.created_at.timestamp_millis())
                    .unwrap_or(card.order.1),
                1,
            );
            cards.push(card);
        }
    }
    for m in &local.msgs {
        stream.push(FeedItem {
            key: CardKey::Local(m.id),
            kind: FeedKind::Agent {
                text: m.text.clone(),
                time: clock(m.at),
            },
            resolved: None,
            order: (1, m.at.timestamp_millis(), 2),
        });
    }

    // Resolution + placement of cards.
    for c in &mut cards {
        if let Some(r) = local.resolved.get(&c.key) {
            c.resolved = Some(r.clone());
        } else if let Some(r) = notification_resolution(d, &c.key) {
            c.resolved = Some(r);
        }
        if !matches!(c.key, CardKey::Plan(_))
            && let Some(Some(t)) = local.seen.get(&c.key)
        {
            c.order = (1, *t, 1);
        }
    }
    for (k, snap) in &local.snapshots {
        if !cards.iter().any(|c| &c.key == k) {
            let mut s = snap.clone();
            if !matches!(k, CardKey::Plan(_))
                && let Some(Some(t)) = local.seen.get(k)
            {
                s.order = (1, *t, 1);
            }
            cards.push(s);
        }
    }

    let mut items: Vec<FeedItem> = Vec::new();
    items.extend(live_card(d));
    items.extend(cards);
    items.extend(stream);
    let at = local.started.unwrap_or(now);
    let digest: Vec<FeedItem> = items
        .iter()
        .filter(|i| i.order.0 == 0 && i.resolved.is_none())
        .cloned()
        .collect();
    items.push(FeedItem {
        key: CardKey::Greeting,
        kind: FeedKind::Agent {
            text: greeting(d, &digest, at),
            time: clock(at),
        },
        resolved: None,
        order: (0, 0, 0),
    });
    items.sort_by_key(|a| a.order);
    items
}

// ---------------------------------------------------------------- filters

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Needs,
    Live,
    Prs,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Needs, Filter::Live, Filter::Prs];
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Needs => "Needs you",
            Filter::Live => "Live",
            Filter::Prs => "PRs",
        }
    }
    pub fn matches(self, it: &FeedItem) -> bool {
        match self {
            Filter::All => true,
            Filter::Needs => it.needs_you(),
            Filter::Live => it.key == CardKey::Live,
            Filter::Prs => it.key == CardKey::Prs,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Counts {
    pub needs: usize,
    pub live: usize,
    pub prs: usize,
}

impl Counts {
    pub fn of(&self, f: Filter) -> usize {
        match f {
            Filter::All => 0,
            Filter::Needs => self.needs,
            Filter::Live => self.live,
            Filter::Prs => self.prs,
        }
    }
}

pub fn counts(items: &[FeedItem]) -> Counts {
    let mut c = Counts::default();
    for it in items {
        if it.needs_you() {
            c.needs += 1;
        }
        match &it.kind {
            FeedKind::Live { used, .. } => c.live = *used,
            FeedKind::Prs { rows, .. } if it.resolved.is_none() => c.prs = rows.len(),
            _ => {}
        }
    }
    c
}

// ---------------------------------------------------------------- resolution texts

/// "Retried · attempt 3/3 running" for a retry of `t` (before the retry).
pub fn retry_text(t: &Task) -> String {
    let cap = t.loop_cap.max(1);
    format!(
        "Retried · attempt {}/{cap} running",
        (t.attempt() + 1).min(cap)
    )
}

/// The planner's follow-up after a retry.
pub fn retry_followup(t: &Task) -> String {
    let cap = t.loop_cap.max(1);
    if t.attempt() + 1 >= cap {
        format!(
            "{} is back with the Builder. Last attempt before I escalate.",
            t.key
        )
    } else {
        format!(
            "{} is back with the Builder. I'll post here when QA has results.",
            t.key
        )
    }
}

/// "Approved · FP-160 is building, 2 tasks queued" (matches the daemon).
pub fn approved_text(tasks: &[Task]) -> String {
    let plural = |n: usize| format!("{n} task{}", if n == 1 { "" } else { "s" });
    let building: Vec<&Task> = tasks.iter().filter(|t| t.state.is_active()).collect();
    let queued = tasks.len() - building.len();
    match building.first() {
        Some(b) if queued > 0 => format!(
            "Approved · {} is building, {} queued",
            b.key,
            plural(queued)
        ),
        Some(b) => format!("Approved · {} is building", b.key),
        None => format!("Approved · {} queued", plural(tasks.len())),
    }
}

pub fn approved_followup(tasks: &[Task]) -> String {
    match tasks.iter().find(|t| t.state.is_active()) {
        Some(t) => {
            let model = t
                .workers
                .get(&WorkerRole::Builder)
                .map(|w| format!(" on {}", short_model(&w.model)))
                .unwrap_or_default();
            format!(
                "Dispatched. {} is with the Builder{model}. I'll post here when QA has results.",
                t.key
            )
        }
        None => "Dispatched. Everything is queued; I'll post here as workers pick it up.".into(),
    }
}

pub fn memory_text(d: &Data, m: &MemoryItem, approve: bool) -> String {
    if approve {
        return "Approved · now visible to the whole company".into();
    }
    let names: Vec<String> = m
        .shared_with
        .iter()
        .filter(|u| Some(**u) != d.me.as_ref().map(|x| x.id))
        .map(|u| first_name(&d.user_name(*u)))
        .collect();
    if names.is_empty() {
        "Kept shared".into()
    } else {
        format!("Kept shared with {}", join_and(&names))
    }
}

/// Suggested replies for the feedback / decide sheet.
pub fn suggestions(d: &Data, t: &Task) -> Vec<String> {
    let det = d.details.get(&t.id);
    let mut out: Vec<String> = Vec::new();
    if t.state == TaskState::Escalated {
        out.push("Follow the reviewer".into());
        out.push("Follow QA".into());
        out.push("Split this into a smaller task".into());
        return out;
    }
    if let Some(v) = det.and_then(|x| x.verdict.as_ref()) {
        for f in v.findings.iter().take(2) {
            out.push(format!("Fix: {}", f.message.trim_end_matches('.')));
        }
    }
    if det
        .and_then(|x| x.qa_report.as_ref())
        .is_some_and(|q| q.tests.failed > 0)
    {
        out.push("Make the failing test pass first".into());
    }
    out.push("Follow the reviewer".into());
    out.truncate(3);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;

    fn seeded() -> (Data, DateTime<Utc>) {
        let now = Utc::now();
        (demo::seed(now), now)
    }

    #[test]
    fn feed_from_demo_state() {
        let (d, now) = seeded();
        let items = assemble(&d, None, &FeedLocal::default(), now, 0);
        assert_eq!(items[0].key, CardKey::Greeting);
        assert_eq!(items[1].key, CardKey::Live);
        let FeedKind::Agent { text, .. } = &items[0].kind else {
            panic!()
        };
        assert!(text.contains("Alex"), "{text}");
        assert!(
            text.contains(
                ": two PRs are ready for review, FP-139 failed QA, FP-136 needs a decision from you"
            ),
            "{text}"
        );
        assert!(text.contains("FP-139 failed QA"), "{text}");
        assert!(text.contains("FP-136 needs a decision from you"), "{text}");
        // Card order: failed, escalated, PRs, memories, then the conversation.
        let keys: Vec<&str> = items
            .iter()
            .map(|i| match &i.kind {
                FeedKind::Agent { .. } => "agent",
                FeedKind::User { .. } => "you",
                FeedKind::Live { .. } => "live",
                FeedKind::TaskCard {
                    kind: TaskCardKind::Failed,
                    ..
                } => "fail",
                FeedKind::TaskCard { .. } => "human",
                FeedKind::Prs { .. } => "prs",
                FeedKind::Memory { .. } => "mem",
                FeedKind::Plan { .. } => "plan",
            })
            .collect();
        assert_eq!(
            &keys[..7],
            &["agent", "live", "fail", "human", "prs", "mem", "mem"]
        );
        assert_eq!(
            *keys.last().unwrap(),
            "plan",
            "plan card follows the message that drafted it"
        );
        let FeedKind::Live {
            used, avg, rows, ..
        } = &items[1].kind
        else {
            panic!()
        };
        assert_eq!(*used, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(*avg, ((22 + 64 + 48 + 30) / 4) as u8);
        let FeedKind::TaskCard { label, dots, .. } = &items[2].kind else {
            panic!()
        };
        assert_eq!(label, "QA failed · 2/3");
        assert_eq!(
            dots.iter().map(|d| d.status).collect::<Vec<_>>(),
            vec![Status::Passed, Status::Failed, Status::Queued]
        );
        let FeedKind::Plan { rows, version, .. } = &items.last().unwrap().kind else {
            panic!()
        };
        assert_eq!(*version, 3);
        assert_eq!(rows[0].id, "T1");
        assert_eq!(rows[0].models, "sonnet-4.5 → gpt-5 → opus-4.1");
    }

    #[test]
    fn chips_filter_and_count() {
        let (d, now) = seeded();
        let items = assemble(&d, None, &FeedLocal::default(), now, 0);
        let c = counts(&items);
        assert_eq!(
            c,
            Counts {
                needs: 5,
                live: 4,
                prs: 2
            }
        );
        let needs: Vec<_> = items.iter().filter(|i| Filter::Needs.matches(i)).collect();
        assert_eq!(needs.len(), 5);
        assert!(needs.iter().all(|i| i.key.needs_you()));
        assert_eq!(items.iter().filter(|i| Filter::Live.matches(i)).count(), 1);
        assert_eq!(items.iter().filter(|i| Filter::Prs.matches(i)).count(), 1);
        assert_eq!(
            items.iter().filter(|i| Filter::All.matches(i)).count(),
            items.len()
        );
    }

    #[test]
    fn resolving_keeps_the_card_with_a_check_line() {
        let (mut d, now) = seeded();
        let mut local = FeedLocal {
            primed: true,
            ..Default::default()
        };
        let items = assemble(&d, None, &local, now, 0);
        local.register(&items, now);
        let fail = items
            .iter()
            .find(|i| {
                matches!(
                    i.kind,
                    FeedKind::TaskCard {
                        kind: TaskCardKind::Failed,
                        ..
                    }
                )
            })
            .unwrap()
            .clone();
        let t = d.task_by_key("FP-139").unwrap().clone();
        assert_eq!(retry_text(&t), "Retried · attempt 3/3 running");
        assert_eq!(
            retry_followup(&t),
            "FP-139 is back with the Builder. Last attempt before I escalate."
        );
        local.resolve(&fail, retry_text(&t));
        local.say(retry_followup(&t), now);
        // The task is retried: it no longer derives a card.
        let mut t2 = t.clone();
        t2.state = TaskState::Testing;
        t2.loop_count += 1;
        d.upsert_task(t2);
        let items = assemble(&d, None, &local, now, 0);
        let kept = items
            .iter()
            .find(|i| i.key == fail.key)
            .expect("resolved card kept");
        assert_eq!(
            kept.resolved.as_deref(),
            Some("Retried · attempt 3/3 running")
        );
        assert!(!kept.needs_you());
        assert_eq!(counts(&items).needs, 4);
        let last = items.last().unwrap();
        assert!(
            matches!(&last.kind, FeedKind::Agent { text, .. } if text.contains("Last attempt")),
            "follow-up at the bottom"
        );
    }

    #[test]
    fn cards_arriving_later_go_to_the_bottom() {
        let (mut d, now) = seeded();
        let mut local = FeedLocal::default();
        let items = assemble(&d, None, &local, now, 0);
        local.register(&items, now);
        local.primed = true;
        let mut t = d.task_by_key("FP-145").unwrap().clone();
        t.state = TaskState::Failed;
        t.last_active = Some(TaskState::Building);
        d.upsert_task(t.clone());
        let later = now + chrono::Duration::minutes(5);
        let items = assemble(&d, None, &local, later, 0);
        assert!(local.clone().register(&items, later));
        local.register(&items, later);
        let items = assemble(&d, None, &local, later, 0);
        assert!(
            matches!(&items.last().unwrap().kind, FeedKind::TaskCard { key, label, .. } if key == "FP-145" && label == "Build failed · 1/3")
        );
    }

    #[test]
    fn notification_resolution_is_picked_up() {
        let (mut d, now) = seeded();
        let mut local = FeedLocal::default();
        let before = assemble(&d, None, &local, now, 0);
        let t = d.task_by_key("FP-139").unwrap().clone();
        d.notifications.push(Notification {
            id: NotificationId::new(),
            user_id: d.me.as_ref().unwrap().id,
            kind: NotificationKind::TaskFailed,
            title: "FP-139 failed QA · 2/3".into(),
            body: String::new(),
            deep_link: "task/FP-139".into(),
            actions: vec![],
            task_id: Some(t.id),
            plan_id: None,
            memory_id: None,
            read: true,
            resolved: Some("✓ Retried · attempt 3/3 running".into()),
            created_at: now,
        });
        let mut t2 = t.clone();
        t2.state = TaskState::Building;
        t2.loop_count += 1;
        d.upsert_task(t2);
        let after = assemble(&d, None, &local, now, 0);
        assert!(local.reconcile(&before, &after, &d));
        let items = assemble(&d, None, &local, now, 0);
        let card = items
            .iter()
            .find(|i| i.key == CardKey::Task(t.id, t.attempt()))
            .unwrap();
        assert_eq!(
            card.resolved.as_deref(),
            Some("Retried · attempt 3/3 running")
        );
    }

    #[test]
    fn approval_and_memory_texts() {
        let (d, _) = seeded();
        let mut ts: Vec<Task> = d.tasks.iter().take(3).cloned().collect();
        ts[0].state = TaskState::Building;
        ts[0].key = "FP-160".into();
        ts[1].state = TaskState::Queued;
        ts[2].state = TaskState::Queued;
        assert_eq!(
            approved_text(&ts),
            "Approved · FP-160 is building, 2 tasks queued"
        );
        assert!(
            approved_followup(&ts)
                .starts_with("Dispatched. FP-160 is with the Builder on sonnet-4.5.")
        );
        let m = d
            .memory
            .iter()
            .find(|m| m.status == MemoryStatus::Pending && !m.shared_with.is_empty())
            .unwrap();
        assert!(memory_text(&d, m, false).starts_with("Kept shared with "));
        assert_eq!(
            memory_text(&d, m, true),
            "Approved · now visible to the whole company"
        );
        let card = memory_card(&d, m);
        let FeedKind::Memory { to, .. } = card.kind else {
            panic!()
        };
        assert_eq!(to, "Company");
    }

    #[test]
    fn suggestions_and_formatting() {
        let (d, now) = seeded();
        let fp139 = d.task_by_key("FP-139").unwrap();
        let s = suggestions(&d, fp139);
        assert!(!s.is_empty() && s.len() <= 3);
        let fp136 = d.task_by_key("FP-136").unwrap();
        assert_eq!(suggestions(&d, fp136)[0], "Follow the reviewer");
        assert_eq!(short_ago(now - chrono::Duration::minutes(3), now), "3m");
        assert_eq!(short_ago(now, now), "now");
        assert_eq!(
            join_and(&["a".into(), "b".into(), "c".into()]),
            "a, b, and c"
        );
        assert!(CardKey::AnyPlan.matches(&CardKey::Plan(PlanId::nil())));
        assert!(!CardKey::AnyPlan.matches(&CardKey::Prs));
    }

    #[test]
    fn voice_messages_are_marked() {
        let (mut d, now) = seeded();
        let tid = current_thread(&d, None).unwrap();
        d.push_message(Message {
            id: MessageId::new(),
            thread_id: tid,
            author: MessageAuthor::User,
            body: "Also log retries at debug level".into(),
            voice: true,
            plan_version: None,
            created_at: now,
        });
        let items = assemble(&d, None, &FeedLocal::default(), now, 0);
        assert!(items.iter().any(
            |i| matches!(&i.kind, FeedKind::User { meta, .. } if meta.starts_with("◉ voice · "))
        ));
    }
}
