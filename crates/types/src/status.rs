//! Task/run state machines and the cross-surface status semantics
//!.

use serde::{Deserialize, Serialize};

/// The five display statuses. Colors and glyphs are identical on every surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Passed,
    Failed,
    NeedsHuman,
    Queued,
}

impl Status {
    /// Dark-theme hex color.
    pub fn hex(self) -> &'static str {
        match self {
            Status::Running => "#5fa8f0",
            Status::Passed => "#4fd19a",
            Status::Failed => "#f0605a",
            Status::NeedsHuman => "#f2c14e",
            Status::Queued => "#7d828b",
        }
    }
    /// Light-theme hex color.
    pub fn hex_light(self) -> &'static str {
        match self {
            Status::Running => "#2f7fd0",
            Status::Passed => "#1f9e6a",
            Status::Failed => "#d43c36",
            Status::NeedsHuman => "#b8860b",
            Status::Queued => "#6b7079",
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Status::Running => "◐",
            Status::Passed => "✓",
            Status::Failed => "✕",
            Status::NeedsHuman => "!",
            Status::Queued => "○",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Passed => "Passed",
            Status::Failed => "Failed",
            Status::NeedsHuman => "Needs human",
            Status::Queued => "Queued",
        }
    }
    /// Tailwind/CSS variable name (`--running`, `--human`, ...).
    pub fn css_var(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::NeedsHuman => "human",
            Status::Queued => "queued",
        }
    }
}

/// Braille spinner frames for `running` in the TUI (advance every 120 ms).
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Task lifecycle (plus `Merged` for the board's Done column).
///
/// ```text
/// Queued → Preparing → Building → Testing → Reviewing → Ready → Merged
///                         ↑          │          │
///                         └── Looping ◄─────────┘   (max loop_cap)
///                                    └──► Escalated (needs human)
/// Any non-terminal state → Cancelled | Failed
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Preparing,
    Building,
    Testing,
    Reviewing,
    Looping,
    Ready,
    Merged,
    Escalated,
    Failed,
    Cancelled,
}

/// Board columns. `Idea` holds draft plans, not tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardColumn {
    Idea,
    Planned,
    Building,
    Qa,
    Review,
    Ready,
    Done,
}

impl BoardColumn {
    pub const ALL: [BoardColumn; 7] = [
        BoardColumn::Idea,
        BoardColumn::Planned,
        BoardColumn::Building,
        BoardColumn::Qa,
        BoardColumn::Review,
        BoardColumn::Ready,
        BoardColumn::Done,
    ];
    pub fn label(self) -> &'static str {
        match self {
            BoardColumn::Idea => "Idea",
            BoardColumn::Planned => "Planned",
            BoardColumn::Building => "Building",
            BoardColumn::Qa => "QA",
            BoardColumn::Review => "Review",
            BoardColumn::Ready => "Ready",
            BoardColumn::Done => "Done",
        }
    }
}

impl TaskState {
    pub const ALL: [TaskState; 11] = [
        TaskState::Queued,
        TaskState::Preparing,
        TaskState::Building,
        TaskState::Testing,
        TaskState::Reviewing,
        TaskState::Looping,
        TaskState::Ready,
        TaskState::Merged,
        TaskState::Escalated,
        TaskState::Failed,
        TaskState::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Preparing => "preparing",
            TaskState::Building => "building",
            TaskState::Testing => "testing",
            TaskState::Reviewing => "reviewing",
            TaskState::Looping => "looping",
            TaskState::Ready => "ready",
            TaskState::Merged => "merged",
            TaskState::Escalated => "escalated",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// No further automatic progress happens from a terminal state.
    /// `Escalated`, `Failed` and `Cancelled` can still be retried by a human.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Ready
                | TaskState::Merged
                | TaskState::Escalated
                | TaskState::Failed
                | TaskState::Cancelled
        )
    }

    /// True while a worker container is (or is about to be) running.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            TaskState::Preparing
                | TaskState::Building
                | TaskState::Testing
                | TaskState::Reviewing
                | TaskState::Looping
        )
    }

    pub fn status(self) -> Status {
        match self {
            TaskState::Queued => Status::Queued,
            TaskState::Preparing
            | TaskState::Building
            | TaskState::Testing
            | TaskState::Reviewing
            | TaskState::Looping => Status::Running,
            TaskState::Ready | TaskState::Merged => Status::Passed,
            TaskState::Escalated => Status::NeedsHuman,
            TaskState::Failed => Status::Failed,
            TaskState::Cancelled => Status::Queued,
        }
    }

    /// Board column. Escalated/Failed/Cancelled stay in the column of the last
    /// active stage, which the caller passes as `last_active`.
    pub fn column(self, last_active: Option<TaskState>) -> BoardColumn {
        match self {
            TaskState::Queued | TaskState::Preparing => BoardColumn::Planned,
            TaskState::Building | TaskState::Looping => BoardColumn::Building,
            TaskState::Testing => BoardColumn::Qa,
            TaskState::Reviewing => BoardColumn::Review,
            TaskState::Ready => BoardColumn::Ready,
            TaskState::Merged => BoardColumn::Done,
            TaskState::Escalated | TaskState::Failed | TaskState::Cancelled => match last_active {
                Some(s)
                    if !matches!(
                        s,
                        TaskState::Escalated | TaskState::Failed | TaskState::Cancelled
                    ) =>
                {
                    s.column(None)
                }
                _ => BoardColumn::Planned,
            },
        }
    }

    /// Whether the state machine allows `self → next`.
    pub fn can_transition(self, next: TaskState) -> bool {
        use TaskState::*;
        if self == next {
            return false;
        }
        match (self, next) {
            // Human actions from any non-final state.
            (s, Cancelled) => !matches!(s, Merged | Cancelled),
            (s, Failed) => !matches!(s, Merged | Failed | Cancelled | Ready),
            (s, Escalated) => s.is_active() || matches!(s, Queued | Failed),
            // Forward path.
            (Queued, Preparing) => true,
            (Preparing, Building) => true,
            (Building, Testing) => true,
            (Testing, Reviewing) => true,
            (Reviewing, Ready) => true,
            (Ready, Merged) => true,
            // Loops.
            (Testing | Reviewing, Looping) => true,
            (Looping, Building) => true,
            // Human feedback on a finished task forces a loop.
            (Ready | Escalated, Looping) => true,
            // Retry / re-queue.
            (Escalated | Failed | Cancelled, Queued) => true,
            // Crash recovery: a run died, re-queue the current stage.
            (Preparing | Building | Testing | Reviewing | Looping, Queued) => true,
            _ => false,
        }
    }
}

/// One execution of one worker role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Pending,
    Starting,
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    /// The daemon restarted and could not re-attach; eligible for one retry.
    Lost,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Starting => "starting",
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::TimedOut => "timed_out",
            RunState::Cancelled => "cancelled",
            RunState::Lost => "lost",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            RunState::Pending,
            RunState::Starting,
            RunState::Running,
            RunState::Succeeded,
            RunState::Failed,
            RunState::TimedOut,
            RunState::Cancelled,
            RunState::Lost,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }
    pub fn is_finished(self) -> bool {
        !matches!(
            self,
            RunState::Pending | RunState::Starting | RunState::Running
        )
    }
    pub fn status(self) -> Status {
        match self {
            RunState::Pending => Status::Queued,
            RunState::Starting | RunState::Running => Status::Running,
            RunState::Succeeded => Status::Passed,
            RunState::Failed | RunState::TimedOut | RunState::Lost => Status::Failed,
            RunState::Cancelled => Status::Queued,
        }
    }
}

/// Worker roles. `Planner` runs in its own read-only container; the other
/// three make up the per-task pipeline. `Distiller` runs memory extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    Planner,
    Builder,
    Qa,
    Reviewer,
    Distiller,
}

impl WorkerRole {
    pub const PIPELINE: [WorkerRole; 3] =
        [WorkerRole::Builder, WorkerRole::Qa, WorkerRole::Reviewer];

    pub fn as_str(self) -> &'static str {
        match self {
            WorkerRole::Planner => "planner",
            WorkerRole::Builder => "builder",
            WorkerRole::Qa => "qa",
            WorkerRole::Reviewer => "reviewer",
            WorkerRole::Distiller => "distiller",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            WorkerRole::Planner,
            WorkerRole::Builder,
            WorkerRole::Qa,
            WorkerRole::Reviewer,
            WorkerRole::Distiller,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }
    pub fn label(self) -> &'static str {
        match self {
            WorkerRole::Planner => "Planner",
            WorkerRole::Builder => "Builder",
            WorkerRole::Qa => "QA",
            WorkerRole::Reviewer => "Reviewer",
            WorkerRole::Distiller => "Distiller",
        }
    }
    /// Only the Builder gets a writable worktree.
    pub fn writes_code(self) -> bool {
        matches!(self, WorkerRole::Builder)
    }
    /// Task state while this role runs.
    pub fn task_state(self) -> Option<TaskState> {
        match self {
            WorkerRole::Builder => Some(TaskState::Building),
            WorkerRole::Qa => Some(TaskState::Testing),
            WorkerRole::Reviewer => Some(TaskState::Reviewing),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_transitions() {
        use TaskState::*;
        let path = [
            Queued, Preparing, Building, Testing, Reviewing, Ready, Merged,
        ];
        for w in path.windows(2) {
            assert!(w[0].can_transition(w[1]), "{:?} -> {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn loop_and_escalate() {
        use TaskState::*;
        assert!(Testing.can_transition(Looping));
        assert!(Reviewing.can_transition(Looping));
        assert!(Looping.can_transition(Building));
        assert!(Testing.can_transition(Escalated));
        assert!(!Building.can_transition(Reviewing));
        assert!(!Merged.can_transition(Cancelled));
        assert!(Escalated.can_transition(Queued));
    }

    #[test]
    fn columns() {
        use TaskState::*;
        assert_eq!(Testing.column(None), BoardColumn::Qa);
        assert_eq!(Escalated.column(Some(Reviewing)), BoardColumn::Review);
        assert_eq!(Failed.column(None), BoardColumn::Planned);
        assert_eq!(Merged.column(None), BoardColumn::Done);
    }

    #[test]
    fn roundtrip_str() {
        for s in TaskState::ALL {
            assert_eq!(TaskState::parse(s.as_str()), Some(s));
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
    }
}
