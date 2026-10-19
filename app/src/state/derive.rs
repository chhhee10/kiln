//! Pure view-model derivations: board columns, card meta lines, the 3-stage
//! worker timeline, memory filters. Surfaces wrap these in `Memo`s.

use super::data::Data;
use super::util::*;
use chrono::{DateTime, Utc};
use kiln_types::event::{LogLevel, LogLine};
use kiln_types::ids::*;
use kiln_types::memory::*;
use kiln_types::*;

// ---------------------------------------------------------------- board

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardFilter {
    pub project: Option<ProjectId>,
    pub people: Vec<UserId>,
}

impl BoardFilter {
    pub fn active(&self) -> bool {
        self.project.is_some() || !self.people.is_empty()
    }
    pub fn toggle_person(&mut self, u: UserId) {
        if let Some(i) = self.people.iter().position(|x| *x == u) {
            self.people.remove(i);
        } else {
            self.people.push(u);
        }
    }
    fn matches(&self, project: ProjectId, owner: UserId) -> bool {
        self.project.is_none_or(|p| p == project)
            && (self.people.is_empty() || self.people.contains(&owner))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CardTarget {
    Task(TaskId),
    Plan {
        plan: PlanId,
        thread: ThreadId,
        owner: UserId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardVm {
    pub dom_key: String,
    pub id_label: String,
    pub project: String,
    pub title: String,
    pub status: Status,
    pub meta: String,
    pub progress: u8,
    pub owner_initials: String,
    pub target: CardTarget,
}

impl CardVm {
    pub fn running(&self) -> bool {
        self.status == Status::Running
    }
    pub fn task_id(&self) -> Option<TaskId> {
        match self.target {
            CardTarget::Task(t) => Some(t),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnVm {
    pub column: BoardColumn,
    pub cards: Vec<CardVm>,
}

/// The short meta line under a card title, in the status color.
pub fn card_meta(t: &Task) -> String {
    let a = format!(
        "{}/{}",
        t.attempt().min(t.loop_cap.max(1)),
        t.loop_cap.max(1)
    );
    let pr = t
        .pr_number
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    match t.state {
        TaskState::Preparing => format!("Preparing · {a}"),
        TaskState::Building | TaskState::Looping => format!("Building · {a}"),
        TaskState::Testing => format!("QA testing · {a}"),
        TaskState::Reviewing => format!("Reviewing · {a}"),
        TaskState::Failed => match t.last_active {
            Some(TaskState::Reviewing) => format!("Review failed · {a}"),
            Some(TaskState::Building) | Some(TaskState::Looping) => format!("Build failed · {a}"),
            _ => format!("QA failed · {a}"),
        },
        TaskState::Escalated => "Needs you".into(),
        TaskState::Queued => "Queued".into(),
        TaskState::Cancelled => "Stopped".into(),
        TaskState::Ready => format!("PR #{pr} ready"),
        TaskState::Merged => format!("Merged #{pr}"),
    }
}

pub fn task_card(d: &Data, t: &Task) -> CardVm {
    CardVm {
        dom_key: t.id.to_string(),
        id_label: t.key.clone(),
        project: d
            .project(t.project_id)
            .map(project_short)
            .unwrap_or_default(),
        title: t.title.clone(),
        status: t.status(),
        meta: card_meta(t),
        progress: t.progress,
        owner_initials: d.user_initials(t.owner_id),
        target: CardTarget::Task(t.id),
    }
}

pub fn idea_card(d: &Data, p: &Plan) -> CardVm {
    CardVm {
        dom_key: p.id.to_string(),
        id_label: format!("draft v{}", p.version),
        project: d
            .project(p.project_id)
            .map(project_short)
            .unwrap_or_default(),
        title: if p.spec.feature.trim().is_empty() {
            "Untitled idea".into()
        } else {
            p.spec.feature.clone()
        },
        status: Status::Queued,
        meta: "Draft".into(),
        progress: 0,
        owner_initials: d.user_initials(p.owner_id),
        target: CardTarget::Plan {
            plan: p.id,
            thread: p.thread_id,
            owner: p.owner_id,
        },
    }
}

pub fn board_columns(d: &Data, f: &BoardFilter) -> Vec<ColumnVm> {
    BoardColumn::ALL
        .into_iter()
        .map(|column| {
            let cards = if column == BoardColumn::Idea {
                let mut ideas: Vec<&Plan> = d
                    .plans
                    .values()
                    .filter(|p| {
                        p.status == PlanStatus::Drafting && f.matches(p.project_id, p.owner_id)
                    })
                    .collect();
                ideas.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
                ideas.into_iter().map(|p| idea_card(d, p)).collect()
            } else {
                d.tasks
                    .iter()
                    .filter(|t| t.column() == column && f.matches(t.project_id, t.owner_id))
                    .map(|t| task_card(d, t))
                    .collect()
            };
            ColumnVm { column, cards }
        })
        .collect()
}

/// "N workers running": tasks with a live worker container.
pub fn running_count(d: &Data) -> usize {
    d.tasks.iter().filter(|t| t.state.is_active()).count()
}

/// Open card count per project for the sidebar: tasks not yet Done plus
/// drafting ideas.
pub fn open_count(d: &Data, project: ProjectId) -> usize {
    d.tasks
        .iter()
        .filter(|t| t.project_id == project && t.column() != BoardColumn::Done)
        .count()
        + d.plans
            .values()
            .filter(|p| p.project_id == project && p.status == PlanStatus::Drafting)
            .count()
}

// ---------------------------------------------------------------- timeline

#[derive(Clone, Debug, PartialEq)]
pub struct StageVm {
    pub role: WorkerRole,
    pub name: &'static str,
    pub status: Status,
    pub label: &'static str,
    pub harness_model: String,
    pub dur: String,
    pub progress: u8,
    /// Connector to the next node is green once this stage passed.
    pub connector_done: bool,
    pub last: bool,
}

/// Index of the pipeline stage the task is in (0 Builder, 1 QA, 2 Reviewer,
/// 3 = all done), or None before building starts.
pub fn stage_index(t: &Task) -> Option<usize> {
    match t.column() {
        BoardColumn::Building => Some(0),
        BoardColumn::Qa => Some(1),
        BoardColumn::Review => Some(2),
        BoardColumn::Ready | BoardColumn::Done => Some(3),
        _ => None,
    }
}

/// The role whose log is live by default.
pub fn current_role(t: &Task) -> WorkerRole {
    match stage_index(t) {
        Some(0) => WorkerRole::Builder,
        Some(2) => WorkerRole::Reviewer,
        _ => WorkerRole::Qa,
    }
}

pub fn harness_label(d: &Data, id: &str) -> String {
    d.harnesses
        .as_ref()
        .and_then(|c| c.harnesses.iter().find(|h| h.id == id))
        .map(|h| h.label.clone())
        .unwrap_or_else(|| id.to_string())
}

fn latest_run(detail: Option<&TaskDetail>, role: WorkerRole) -> Option<&Run> {
    detail?
        .runs
        .iter()
        .filter(|r| r.role == role)
        .max_by_key(|r| (r.attempt, r.started_at))
}

pub fn stages(d: &Data, t: &Task, now: DateTime<Utc>) -> Vec<StageVm> {
    let idx = stage_index(t);
    let detail = d.details.get(&t.id);
    WorkerRole::PIPELINE
        .into_iter()
        .enumerate()
        .map(|(i, role)| {
            let status = match idx {
                None => Status::Queued,
                Some(x) if i < x => Status::Passed,
                Some(x) if i == x => t.status(),
                _ => Status::Queued,
            };
            let run = latest_run(detail, role);
            let hm = t
                .workers
                .get(&role)
                .map(|w| format!("{} · {}", harness_label(d, &w.harness), w.model))
                .or_else(|| run.map(|r| format!("{} · {}", harness_label(d, &r.harness), r.model)))
                .unwrap_or_default();
            let elapsed = run.and_then(|r| {
                r.started_at
                    .map(|s| (r.ended_at.unwrap_or(now) - s).num_seconds())
            });
            let progress = match status {
                Status::Passed => 100,
                _ if Some(i) == idx => t.progress,
                _ => 0,
            };
            let dur = match status {
                Status::Passed => elapsed.map(fmt_duration).unwrap_or_else(|| "done".into()),
                Status::Queued => {
                    if t.state == TaskState::Cancelled && Some(i) == idx {
                        "stopped".into()
                    } else {
                        "waiting".into()
                    }
                }
                Status::Running => {
                    format!("{}% · {}", t.progress, fmt_duration(elapsed.unwrap_or(0)))
                }
                Status::Failed => format!("failed after {}", fmt_duration(elapsed.unwrap_or(0))),
                Status::NeedsHuman => "escalated".into(),
            };
            StageVm {
                role,
                name: role.label(),
                status,
                label: status.label(),
                harness_model: hm,
                dur,
                progress,
                connector_done: status == Status::Passed,
                last: i == 2,
            }
        })
        .collect()
}

/// Log lines for one role, newest last, at most `n`.
pub fn log_tail(d: &Data, task: TaskId, role: WorkerRole, n: usize) -> Vec<LogLine> {
    let Some(lines) = d.logs.get(&task) else {
        return vec![];
    };
    let mut v: Vec<LogLine> = lines
        .iter()
        .rev()
        .filter(|l| l.role == role)
        .take(n)
        .cloned()
        .collect();
    v.reverse();
    v
}

pub fn level_var(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Info => "dim",
        LogLevel::Step => "text",
        LogLevel::Pass => "passed",
        LogLevel::Warn => "human",
        LogLevel::Fail => "failed",
        LogLevel::Capt => "running",
    }
}

// ---------------------------------------------------------------- memory

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrainFilter {
    All,
    Private,
    Shared,
    Company,
    Queue,
}

impl BrainFilter {
    pub const ALL: [BrainFilter; 5] = [
        BrainFilter::All,
        BrainFilter::Private,
        BrainFilter::Shared,
        BrainFilter::Company,
        BrainFilter::Queue,
    ];
    pub fn label(self) -> &'static str {
        match self {
            BrainFilter::All => "All memories",
            BrainFilter::Private => "Private",
            BrainFilter::Shared => "Shared with people",
            BrainFilter::Company => "Company",
            BrainFilter::Queue => "Approval queue",
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            BrainFilter::All => "∗",
            BrainFilter::Private => Visibility::Private.glyph(),
            BrainFilter::Shared => Visibility::Shared.glyph(),
            BrainFilter::Company => Visibility::Company.glyph(),
            BrainFilter::Queue => "!",
        }
    }
    pub fn matches(self, m: &MemoryItem) -> bool {
        let pending = m.status == MemoryStatus::Pending;
        match self {
            BrainFilter::All => true,
            BrainFilter::Private => m.visibility == Visibility::Private,
            BrainFilter::Shared => m.visibility == Visibility::Shared,
            BrainFilter::Company => m.visibility == Visibility::Company && !pending,
            BrainFilter::Queue => pending,
        }
    }
}

/// Visible memory list for a filter + search query (archived/rejected hidden).
pub fn memory_list(d: &Data, f: BrainFilter, q: &str) -> Vec<MemoryItem> {
    let q = q.trim().to_lowercase();
    d.memory
        .iter()
        .filter(|m| {
            !matches!(
                m.status,
                MemoryStatus::Archived | MemoryStatus::Rejected | MemoryStatus::Superseded
            )
        })
        .filter(|m| f.matches(m))
        .filter(|m| {
            q.is_empty() || {
                let hay =
                    format!("{} {} {} {}", m.title, m.body, m.subject, m.object).to_lowercase();
                q.split_whitespace().all(|w| hay.contains(w))
            }
        })
        .cloned()
        .collect()
}

pub fn memory_count(d: &Data, f: BrainFilter) -> usize {
    memory_list(d, f, "").len()
}

/// "◑ Shared · AR, JL" style label for a row.
pub fn visibility_label(d: &Data, m: &MemoryItem) -> String {
    match m.visibility {
        Visibility::Shared if !m.shared_with.is_empty() => {
            let who: Vec<String> = m.shared_with.iter().map(|u| d.user_initials(*u)).collect();
            format!("Shared · {}", who.join(", "))
        }
        v => v.label().to_string(),
    }
}

/// Tags shown on the detail pane: the words of the item's subject (the
/// extractor writes short topical subjects), else its kind and project.
pub fn memory_tags(d: &Data, m: &MemoryItem) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    if m.subject.len() <= 40 {
        for w in m
            .subject
            .split([' ', ','])
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty())
        {
            if !tags.contains(&w) {
                tags.push(w);
            }
        }
    }
    if tags.is_empty() {
        tags.push(m.kind.as_str().to_string());
        if let Some(p) = m.project_id.and_then(|p| d.project(p)) {
            tags.push(project_short(p).to_lowercase());
        }
    }
    tags
}

pub fn visibility_help(m: &MemoryItem) -> &'static str {
    if m.kind == MemoryType::Preference {
        return "Personal preferences are always Private and can't be shared.";
    }
    if m.status == MemoryStatus::Pending {
        return "Waiting for an admin to approve. Until then it stays visible only to its previous audience.";
    }
    match m.visibility {
        Visibility::Private => "Only you and workers on your runs can use this.",
        Visibility::Shared => "Visible to the people selected below and to their runs.",
        Visibility::Company => "Every teammate, planner and worker can use this.",
    }
}

// ---------------------------------------------------------------- palette

/// Case-insensitive substring filter on "group label" (design: max 9 rows).
pub fn palette_filter(items: &[(String, String)], q: &str, max: usize) -> Vec<usize> {
    let q = q.trim().to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, (g, l))| q.is_empty() || format!("{g} {l}").to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .take(max)
        .collect()
}

// ---------------------------------------------------------------- credentials

pub fn credential_status(c: &CredentialSummary) -> (String, Status) {
    match &c.status {
        CredentialHealth::Untested => ("Untested".into(), Status::Queued),
        CredentialHealth::Valid => ("Valid".into(), Status::Passed),
        CredentialHealth::Reachable { latency_ms } => {
            (format!("Reachable · {latency_ms}ms"), Status::Passed)
        }
        CredentialHealth::ExpiresSoon { days } => (
            format!(
                "Connected · expires in {days} day{}",
                if *days == 1 { "" } else { "s" }
            ),
            Status::NeedsHuman,
        ),
        CredentialHealth::Invalid { error } => (format!("Invalid · {error}"), Status::Failed),
    }
}

/// Hidden form of a masked credential: the API's masked value with the
/// visible middle replaced by dots. Reveal shows `masked` as returned —
/// secrets are never fetched.
pub fn credential_hidden(c: &CredentialSummary) -> String {
    let m = &c.masked;
    let dotted = match m.find('…') {
        Some(i) => format!("{}••••••••••••{}", &m[..i], &m[i + '…'.len_utf8()..]),
        None => {
            let tail: String = m
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("••••••••••••{tail}")
        }
    };
    match &c.base_url {
        Some(u) => format!("{u} · {dotted}"),
        None => dotted,
    }
}

pub fn credential_revealed(c: &CredentialSummary) -> String {
    let mut s = match &c.base_url {
        Some(u) => format!("{u} · {}", c.masked),
        None => c.masked.clone(),
    };
    if let Some(e) = &c.env_var {
        s.push_str(&format!(" · ${e}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;

    fn seeded() -> Data {
        demo::seed(Utc::now())
    }

    #[test]
    fn columns_match_prototype_layout() {
        let d = seeded();
        let cols = board_columns(&d, &BoardFilter::default());
        let counts: Vec<usize> = cols.iter().map(|c| c.cards.len()).collect();
        // Idea holds the 2 teammates' drafts + the current planner draft.
        assert_eq!(counts, vec![3, 2, 2, 2, 2, 2, 3]);
        let qa = &cols[3];
        assert!(
            qa.cards
                .iter()
                .any(|c| c.id_label == "FP-142" && c.meta == "QA testing · 2/3")
        );
        assert!(
            qa.cards
                .iter()
                .any(|c| c.id_label == "FP-139" && c.meta == "QA failed · 2/3")
        );
        let review = &cols[4];
        assert!(review.cards.iter().any(|c| c.meta == "Needs you"));
        assert!(
            cols[5]
                .cards
                .iter()
                .all(|c| c.meta.starts_with("PR #") && c.meta.ends_with("ready"))
        );
        assert!(cols[6].cards.iter().all(|c| c.meta.starts_with("Merged #")));
    }

    #[test]
    fn filters() {
        let d = seeded();
        let cli = d
            .projects
            .iter()
            .find(|p| p.name.ends_with("CLI/SDK"))
            .unwrap()
            .id;
        let f = BoardFilter {
            project: Some(cli),
            people: vec![],
        };
        assert!(f.active());
        let cols = board_columns(&d, &f);
        assert!(
            cols.iter()
                .flat_map(|c| &c.cards)
                .all(|c| c.project == "CLI/SDK")
        );
        let ar = d.me.as_ref().unwrap().id;
        let mut f = BoardFilter::default();
        f.toggle_person(ar);
        let cols = board_columns(&d, &f);
        assert!(
            cols.iter()
                .flat_map(|c| &c.cards)
                .all(|c| c.owner_initials == "AR")
        );
        f.toggle_person(ar);
        assert!(!f.active());
    }

    #[test]
    fn counts() {
        let d = seeded();
        assert_eq!(running_count(&d), 4);
        let cli = d
            .projects
            .iter()
            .find(|p| p.name.ends_with("CLI/SDK"))
            .unwrap()
            .id;
        assert_eq!(open_count(&d, cli), 6);
    }

    #[test]
    fn timeline_for_qa_task() {
        let d = seeded();
        let t = d.task_by_key("FP-142").unwrap();
        let s = stages(&d, t, Utc::now());
        assert_eq!(s[0].status, Status::Passed);
        assert_eq!(s[1].status, Status::Running);
        assert!(s[1].dur.starts_with("48% · "));
        assert_eq!(s[2].status, Status::Queued);
        assert_eq!(s[2].dur, "waiting");
        assert!(s[0].connector_done && !s[1].connector_done && s[2].last);
        assert_eq!(s[0].harness_model, "Claude Agent SDK · claude-sonnet-4.5");
        assert_eq!(current_role(t), WorkerRole::Qa);
    }

    #[test]
    fn card_meta_for_stopped_and_failed() {
        let d = seeded();
        let mut t = d.task_by_key("FP-145").unwrap().clone();
        t.state = TaskState::Cancelled;
        t.last_active = Some(TaskState::Building);
        assert_eq!(card_meta(&t), "Stopped");
        assert_eq!(t.column(), BoardColumn::Building);
        t.state = TaskState::Failed;
        assert_eq!(card_meta(&t), "Build failed · 1/3");
    }

    #[test]
    fn memory_filters_and_search() {
        let d = seeded();
        assert_eq!(memory_count(&d, BrainFilter::All), 7);
        assert_eq!(memory_count(&d, BrainFilter::Queue), 2);
        assert_eq!(memory_count(&d, BrainFilter::Private), 1);
        assert_eq!(memory_list(&d, BrainFilter::All, "jitter").len(), 2);
        assert!(memory_list(&d, BrainFilter::All, "nothing-matches-this").is_empty());
        let m = d
            .memory
            .iter()
            .find(|m| m.visibility == Visibility::Shared && m.status == MemoryStatus::Active)
            .unwrap();
        assert!(visibility_label(&d, m).starts_with("Shared · "));
    }

    #[test]
    fn palette_matching() {
        let items = vec![
            ("Go to".to_string(), "Board".to_string()),
            ("Go to".to_string(), "Planner".to_string()),
            ("Task".to_string(), "Retry FP-142".to_string()),
        ];
        assert_eq!(palette_filter(&items, "", 9), vec![0, 1, 2]);
        assert_eq!(palette_filter(&items, "go to p", 9), vec![1]);
        assert_eq!(palette_filter(&items, "fp-142", 9), vec![2]);
        assert_eq!(palette_filter(&items, "", 2).len(), 2);
    }

    #[test]
    fn credential_masking_never_invents_secrets() {
        let d = seeded();
        for c in &d.credentials {
            let hidden = credential_hidden(c);
            let shown = credential_revealed(c);
            assert!(shown.contains(&c.masked));
            assert!(hidden.contains("••••"));
            // The last four characters are all either view ever shows.
            let tail: String = c.masked.chars().rev().take(4).collect();
            assert!(hidden.chars().rev().collect::<String>().starts_with(&tail));
        }
    }
}
